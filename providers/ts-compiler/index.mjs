#!/usr/bin/env node
// CodeQuest native semantic provider for TypeScript / TSX using the official
// TypeScript compiler API (ts.createProgram + program.getTypeChecker).
// Emits the codequest-hir-export v1 JSON contract with exact UTF-8 byte spans.

import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

function fail(message) {
  process.stderr.write(`cq-ts-compiler: ${message}\n`);
  process.exit(1);
}

function loadTypeScript(input) {
  const candidates = [];
  if (input.typescript_path) candidates.push(input.typescript_path);
  if (process.env.CQ_TS_MODULE) candidates.push(process.env.CQ_TS_MODULE);

  for (const cand of candidates) {
    try {
      const req = createRequire(import.meta.url);
      return req(path.resolve(cand));
    } catch {
      // try next
    }
  }

  const searchAnchors = [];
  for (const project of input.projects || []) {
    if (project.tsconfig) searchAnchors.push(project.tsconfig);
  }
  if (input.root) searchAnchors.push(path.join(input.root, 'package.json'));
  searchAnchors.push(import.meta.url);

  for (const anchor of searchAnchors) {
    try {
      const req = createRequire(anchor);
      const resolved = req.resolve('typescript');
      return req(resolved);
    } catch {
      // try next
    }
  }

  fail(
    'cannot resolve the "typescript" compiler module; install project dependencies (npm install) or set CQ_TS_MODULE=/path/to/node_modules/typescript',
  );
}

/**
 * Build a UTF-16 code unit index -> UTF-8 byte offset map of length text.length + 1.
 * Every mapped offset is guaranteed to be a valid UTF-8 character boundary.
 */
function buildUtf16ToUtf8(text) {
  const map = new Int32Array(text.length + 1);
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    map[i] = bytes;
    const code = text.charCodeAt(i);
    if (code < 0x80) {
      bytes += 1;
    } else if (code < 0x800) {
      bytes += 2;
    } else if (code >= 0xd800 && code <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        map[i + 1] = bytes;
        bytes += 4;
        i += 1;
        continue;
      }
      bytes += 3;
    } else {
      bytes += 3;
    }
  }
  map[text.length] = bytes;
  return map;
}

function isInsideDir(filePath, dirPath) {
  const rel = path.relative(dirPath, filePath);
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}

function isNodeModulesPath(filePath) {
  const parts = filePath.split(path.sep);
  return parts.includes('node_modules');
}

function stripExt(relPath) {
  return relPath.replace(/\.(d\.ts|tsx|ts|mts|cts|jsx|js|mjs|cjs)$/i, '');
}

function run() {
  const args = process.argv.slice(2);
  if (args.length < 2) {
    fail('usage: node providers/ts-compiler/index.mjs INPUT.json OUTPUT.json');
  }
  const [inputPath, outputPath] = args;
  const input = JSON.parse(fs.readFileSync(inputPath, 'utf8'));
  const ts = loadTypeScript(input);

  const root = path.resolve(input.root || '.');
  const projects = input.projects || [];

  const rawFiles = [];
  const symbols = [];
  const references = [];
  const parents = [];
  const relations = [];
  const bodies = [];
  const calls = [];
  const parameters = [];
  const comments = [];
  const diagnostics = [];

  let nextFileId = 1;
  let nextToken = 1;

  for (const project of projects) {
    const unit = project.unit || 'ts-project';
    const tsconfigPath = path.resolve(project.tsconfig);
    const projectDir = path.dirname(tsconfigPath);

    // Map canonical logical path -> materialized frozen path on disk (if provided).
    const materializedMap = new Map();
    if (project.files && typeof project.files === 'object') {
      for (const [logical, mat] of Object.entries(project.files)) {
        materializedMap.set(path.resolve(logical), path.resolve(mat));
      }
    }

    const readFrozenOrDisk = (fileName) => {
      const abs = path.resolve(fileName);
      const mat = materializedMap.get(abs);
      if (mat) {
        try {
          return fs.readFileSync(mat, 'utf8');
        } catch {
          return undefined;
        }
      }
      try {
        return fs.readFileSync(abs, 'utf8');
      } catch {
        return undefined;
      }
    };

    const configFile = ts.readConfigFile(tsconfigPath, readFrozenOrDisk);
    if (configFile.error) {
      diagnostics.push({
        severity: 'error',
        message: ts.flattenDiagnosticMessageText(configFile.error.messageText, '\n'),
      });
      continue;
    }

    const parseConfigHost = {
      useCaseSensitiveFileNames: ts.sys.useCaseSensitiveFileNames,
      readDirectory: ts.sys.readDirectory,
      fileExists: (f) => materializedMap.has(path.resolve(f)) || ts.sys.fileExists(f),
      readFile: readFrozenOrDisk,
    };

    const parsed = ts.parseJsonConfigFileContent(
      configFile.config,
      parseConfigHost,
      projectDir,
      { noEmit: true },
      tsconfigPath,
    );

    for (const err of parsed.errors || []) {
      diagnostics.push({
        severity: err.category === ts.DiagnosticCategory.Error ? 'error' : 'warning',
        message: ts.flattenDiagnosticMessageText(err.messageText, '\n'),
      });
    }

    const defaultHost = ts.createCompilerHost(parsed.options, true);
    const compilerHost = {
      ...defaultHost,
      fileExists: (fileName) =>
        materializedMap.has(path.resolve(fileName)) || defaultHost.fileExists(fileName),
      readFile: (fileName) => {
        const abs = path.resolve(fileName);
        if (materializedMap.has(abs)) {
          return readFrozenOrDisk(abs);
        }
        return defaultHost.readFile(fileName);
      },
      getSourceFile: (fileName, languageVersion, onError, shouldCreateNewSourceFile) => {
        const abs = path.resolve(fileName);
        if (materializedMap.has(abs)) {
          const text = readFrozenOrDisk(abs);
          if (text !== undefined) {
            return ts.createSourceFile(fileName, text, languageVersion, true);
          }
        }
        return defaultHost.getSourceFile(
          fileName,
          languageVersion,
          onError,
          shouldCreateNewSourceFile,
        );
      },
    };

    const program = ts.createProgram({
      rootNames: parsed.fileNames,
      options: parsed.options,
      host: compilerHost,
    });
    const checker = program.getTypeChecker();

    // Register project source files (excluding node_modules and TS default libs).
    const sfInfo = new Map(); // SourceFile -> { id, absPath, relPath, moduleName, utf8Map }
    const projectSourceFiles = [];
    for (const sf of program.getSourceFiles()) {
      const absPath = path.resolve(sf.fileName);
      if (isNodeModulesPath(absPath)) continue;
      if (!isInsideDir(absPath, root) && !isInsideDir(absPath, projectDir)) continue;
      if (program.isSourceFileDefaultLibrary(sf)) continue;

      const fileId = nextFileId++;
      const relPath = path.relative(root, absPath).split(path.sep).join('/');
      const utf8Map = buildUtf16ToUtf8(sf.text);
      const info = {
        id: fileId,
        absPath,
        relPath,
        moduleName: stripExt(relPath),
        utf8Map,
      };
      sfInfo.set(sf, info);
      projectSourceFiles.push(sf);

      const fileEntry = { id: fileId, path: absPath, text: sf.text };
      if (materializedMap.size === 0 || materializedMap.has(absPath)) {
        fileEntry.unit = unit;
      }
      rawFiles.push(fileEntry);
    }

    // Diagnostics: syntactic errors are strict errors; semantic diagnostics are
    // warnings so intra-project graphs still publish when node_modules is absent.
    let semanticWarningCount = 0;
    const maxSemanticMessages = 10;
    for (const sf of projectSourceFiles) {
      for (const diag of program.getSyntacticDiagnostics(sf)) {
        diagnostics.push({
          severity: 'error',
          message: `${sfInfo.get(sf).relPath}: ${ts.flattenDiagnosticMessageText(diag.messageText, ' ')}`,
        });
      }
      for (const diag of program.getSemanticDiagnostics(sf)) {
        semanticWarningCount += 1;
        if (semanticWarningCount <= maxSemanticMessages) {
          diagnostics.push({
            severity: 'warning',
            message: `${sfInfo.get(sf).relPath} (TS${diag.code}): ${ts.flattenDiagnosticMessageText(diag.messageText, ' ')}`,
          });
        }
      }
    }
    if (semanticWarningCount > maxSemanticMessages) {
      diagnostics.push({
        severity: 'warning',
        message: `${unit}: ${semanticWarningCount - maxSemanticMessages} additional TypeScript semantic diagnostics omitted (total ${semanticWarningCount})`,
      });
    }

    const spanOf = (sf, start16, end16) => {
      const info = sfInfo.get(sf);
      if (!info) return null;
      const len = sf.text.length;
      const s = Math.max(0, Math.min(start16, len));
      const e = Math.max(s, Math.min(end16, len));
      return { file: info.id, start: info.utf8Map[s], end: info.utf8Map[e] };
    };

    const nodeSpan = (sf, node) => {
      if (!node) return null;
      return spanOf(sf, node.getStart(sf), node.getEnd());
    };

    const resolveAlias = (sym) => {
      if (!sym) return null;
      let cur = sym;
      const seen = new Set();
      while (cur && (cur.flags & ts.SymbolFlags.Alias) !== 0 && !seen.has(cur)) {
        seen.add(cur);
        try {
          const next = checker.getAliasedSymbol(cur);
          if (!next || next === cur || next.flags === 0 || next.escapedName === 'unknown') {
            break;
          }
          cur = next;
        } catch {
          break;
        }
      }
      return cur;
    };

    const exportedSymbols = new Set();
    for (const sf of projectSourceFiles) {
      const modSym = checker.getSymbolAtLocation(sf);
      if (modSym) {
        try {
          for (const exp of checker.getExportsOfModule(modSym)) {
            exportedSymbols.add(exp);
            const target = resolveAlias(exp);
            if (target) exportedSymbols.add(target);
          }
        } catch {
          // ignore
        }
      }
    }

    // Per-project symbol registry.
    const symToToken = new Map(); // ts.Symbol -> token (int)
    const externalKeyToToken = new Map(); // external key -> token (dedup unresolved imports)
    const tokenRecord = new Map(); // token -> symbol obj
    const symMeta = new Map(); // ts.Symbol -> { key, fileId, defSpan, bodySpan }
    const usedKeys = new Set();
    const parentOf = new Map(); // childToken -> parentToken
    const declNodeToToken = new Map(); // primary declaration node -> token
    const namedFunctionNodes = new Set(); // ArrowFunction / FunctionExpression already emitted as named Function/Method
    const callCalleeMap = new Map(); // AST node -> call index
    const newCalleeSet = new Set(); // AST node that is callee of NewExpression
    const emittedDeclNames = new Set(); // AST identifier nodes that are declaration names (not references)
    const projectClosures = []; // collected closures sorted outer-to-inner before pushing to bodies

    const allocateUniqueKey = (baseKey) => {
      if (!usedKeys.has(baseKey)) {
        usedKeys.add(baseKey);
        return baseKey;
      }
      let n = 2;
      while (usedKeys.has(`${baseKey}#${n}`)) n += 1;
      const key = `${baseKey}#${n}`;
      usedKeys.add(key);
      return key;
    };

    const unwrapFunctionInitializer = (init) => {
      if (!init) return null;
      let expr = init;
      while (ts.isParenthesizedExpression(expr) || ts.isAsExpression(expr) || ts.isSatisfiesExpression(expr)) {
        expr = expr.expression;
      }
      if (ts.isArrowFunction(expr) || ts.isFunctionExpression(expr)) {
        return expr;
      }
      // React memo(...) / forwardRef(...)
      if (ts.isCallExpression(expr) && expr.arguments.length >= 1) {
        let calleeName = '';
        if (ts.isIdentifier(expr.expression)) calleeName = expr.expression.text;
        else if (ts.isPropertyAccessExpression(expr.expression)) calleeName = expr.expression.name.text;
        if (calleeName === 'memo' || calleeName === 'forwardRef') {
          let arg = expr.arguments[0];
          while (ts.isParenthesizedExpression(arg) || ts.isAsExpression(arg)) arg = arg.expression;
          if (ts.isArrowFunction(arg) || ts.isFunctionExpression(arg)) {
            return arg;
          }
        }
      }
      return null;
    };

    const hasModifier = (node, kind) => {
      if (!ts.canHaveModifiers(node)) return false;
      const mods = ts.getModifiers(node);
      return Boolean(mods && mods.some((m) => m.kind === kind));
    };

    const isNodeExported = (decl, sym) => {
      if (sym && exportedSymbols.has(sym)) return true;
      if (!decl) return false;
      if (hasModifier(decl, ts.SyntaxKind.ExportKeyword)) return true;
      if (ts.isVariableDeclaration(decl) && decl.parent && ts.isVariableDeclarationList(decl.parent)) {
        const stmt = decl.parent.parent;
        if (stmt && hasModifier(stmt, ts.SyntaxKind.ExportKeyword)) return true;
      }
      return false;
    };

    const isTestFile = (relPath) =>
      /\.(test|spec)\.(ts|tsx|mts|cts|js|jsx)$/i.test(relPath) ||
      /(^|\/)(tests?|__tests__)\//.test(relPath);

    const pickPrimaryDecl = (sym) => {
      if (!sym) return null;
      const decls = sym.declarations || [];
      // Prefer project declarations with a body (implementation over overload).
      let best = null;
      for (const d of decls) {
        const sf = d.getSourceFile();
        if (!sfInfo.has(sf)) continue;
        if (
          (ts.isFunctionDeclaration(d) || ts.isMethodDeclaration(d) || ts.isConstructorDeclaration(d)) &&
          d.body
        ) {
          return d;
        }
        if (!best) best = d;
      }
      if (best) return best;
      return sym.valueDeclaration || decls[0] || null;
    };

    const isNamedFieldOwner = (decl) => {
      if (!decl || !decl.parent) return false;
      const p = decl.parent;
      if (ts.isClassDeclaration(p) || ts.isInterfaceDeclaration(p)) return true;
      if (ts.isTypeLiteralNode(p) && p.parent && ts.isTypeAliasDeclaration(p.parent)) return true;
      if (
        ts.isObjectLiteralExpression(p) &&
        p.parent &&
        ts.isVariableDeclaration(p.parent) &&
        p.parent.parent &&
        ts.isVariableDeclarationList(p.parent.parent) &&
        p.parent.parent.parent &&
        ts.isVariableStatement(p.parent.parent.parent) &&
        ts.isSourceFile(p.parent.parent.parent.parent)
      ) {
        return true;
      }
      return false;
    };

    const isImportDecl = (decl) =>
      decl !== null &&
      (ts.isImportSpecifier(decl) ||
        ts.isImportClause(decl) ||
        ts.isNamespaceImport(decl) ||
        ts.isImportEqualsDeclaration(decl));

    const importModuleSpecifier = (decl) => {
      let cur = decl;
      while (cur) {
        if (ts.isImportDeclaration(cur) && ts.isStringLiteral(cur.moduleSpecifier)) {
          return cur.moduleSpecifier.text;
        }
        cur = cur.parent;
      }
      return 'external';
    };

    const kindForSymbol = (sym, decl) => {
      if (decl) {
        if (ts.isFunctionDeclaration(decl) || ts.isFunctionExpression(decl) || ts.isArrowFunction(decl)) {
          return 'Function';
        }
        if (
          ts.isMethodDeclaration(decl) ||
          ts.isMethodSignature(decl) ||
          ts.isGetAccessorDeclaration(decl) ||
          ts.isSetAccessorDeclaration(decl) ||
          ts.isConstructorDeclaration(decl)
        ) {
          return 'Method';
        }
        if (ts.isClassDeclaration(decl) || ts.isClassExpression(decl)) return 'Class';
        if (ts.isInterfaceDeclaration(decl)) return 'Interface';
        if (ts.isTypeAliasDeclaration(decl)) return 'TypeAlias';
        if (ts.isEnumDeclaration(decl)) return 'Enum';
        if (ts.isEnumMember(decl)) return 'EnumMember';
        if (ts.isPropertyDeclaration(decl) || ts.isPropertySignature(decl)) {
          return isNamedFieldOwner(decl) ? 'Field' : 'Variable';
        }
        if (ts.isPropertyAssignment(decl)) {
          if (isNamedFieldOwner(decl)) {
            return unwrapFunctionInitializer(decl.initializer) ? 'Method' : 'Field';
          }
          return 'Variable';
        }
        if (ts.isShorthandPropertyAssignment(decl)) return 'Variable';
        if (ts.isParameter(decl)) return 'Parameter';
        if (ts.isModuleDeclaration(decl)) return 'Module';
        if (ts.isTypeParameterDeclaration(decl)) return 'TypeParameter';
        if (ts.isVariableDeclaration(decl)) {
          if (unwrapFunctionInitializer(decl.initializer)) return 'Function';
          const isConst =
            decl.parent &&
            ts.isVariableDeclarationList(decl.parent) &&
            (decl.parent.flags & ts.NodeFlags.Const) !== 0;
          const isTopLevel =
            decl.parent &&
            decl.parent.parent &&
            ts.isVariableStatement(decl.parent.parent) &&
            ts.isSourceFile(decl.parent.parent.parent);
          return isConst && isTopLevel ? 'Constant' : 'Variable';
        }
        if (ts.isBindingElement(decl)) {
          // Check if inside a parameter vs local variable.
          let p = decl.parent;
          while (p && (ts.isObjectBindingPattern(p) || ts.isArrayBindingPattern(p) || ts.isBindingElement(p))) {
            p = p.parent;
          }
          if (p && ts.isParameter(p)) return 'Parameter';
          return 'Variable';
        }
      }
      const f = sym.flags;
      if (f & ts.SymbolFlags.Function) return 'Function';
      if (f & ts.SymbolFlags.Method) return 'Method';
      if (f & ts.SymbolFlags.Class) return 'Class';
      if (f & ts.SymbolFlags.Interface) return 'Interface';
      if (f & ts.SymbolFlags.TypeAlias) return 'TypeAlias';
      if (f & ts.SymbolFlags.Enum) return 'Enum';
      if (f & ts.SymbolFlags.EnumMember) return 'EnumMember';
      if (f & ts.SymbolFlags.Property) return 'Variable';
      if (f & ts.SymbolFlags.NamespaceModule || f & ts.SymbolFlags.ValueModule) return 'Module';
      if (f & ts.SymbolFlags.TypeParameter) return 'TypeParameter';
      if (f & ts.SymbolFlags.Variable) return 'Variable';
      return 'Symbol';
    };

    const isLocalDecl = (decl) => {
      if (!decl) return false;
      if (ts.isParameter(decl) || ts.isTypeParameterDeclaration(decl)) return true;
      if (
        ts.isPropertySignature(decl) ||
        ts.isPropertyDeclaration(decl) ||
        ts.isPropertyAssignment(decl) ||
        ts.isShorthandPropertyAssignment(decl)
      ) {
        return !isNamedFieldOwner(decl);
      }
      if (ts.isBindingElement(decl)) {
        let p = decl.parent;
        while (p) {
          if (ts.isParameter(p) || ts.isBlock(p) || ts.isFunctionLike(p)) return true;
          if (ts.isSourceFile(p)) return false;
          p = p.parent;
        }
        return true;
      }
      if (ts.isVariableDeclaration(decl)) {
        const list = decl.parent;
        const stmt = list && list.parent;
        if (stmt && ts.isVariableStatement(stmt) && ts.isSourceFile(stmt.parent)) {
          return false;
        }
        return true;
      }
      return false;
    };

    const qualifiedNameFor = (sym, decl, name) => {
      const sf = decl ? decl.getSourceFile() : null;
      const info = sf ? sfInfo.get(sf) : null;
      const parts = [];
      if (decl) {
        let cur = decl.parent;
        while (cur && !ts.isSourceFile(cur)) {
          if (
            (ts.isClassDeclaration(cur) ||
              ts.isInterfaceDeclaration(cur) ||
              ts.isEnumDeclaration(cur) ||
              ts.isModuleDeclaration(cur) ||
              ts.isFunctionDeclaration(cur) ||
              ts.isMethodDeclaration(cur)) &&
            cur.name &&
            ts.isIdentifier(cur.name)
          ) {
            parts.push(cur.name.text);
          } else if (
            ts.isVariableDeclaration(cur) &&
            ts.isIdentifier(cur.name) &&
            cur.parent &&
            ts.isVariableDeclarationList(cur.parent) &&
            cur.parent.parent &&
            ts.isVariableStatement(cur.parent.parent) &&
            ts.isSourceFile(cur.parent.parent.parent)
          ) {
            parts.push(cur.name.text);
          }
          cur = cur.parent;
        }
      }
      parts.reverse();
      parts.push(name);
      if (info) {
        return `${info.moduleName}::${parts.join('::')}`;
      }
      const fqn = (() => {
        try {
          return checker.getFullyQualifiedName(sym);
        } catch {
          return name;
        }
      })();
      return fqn || name;
    };

    const signatureFor = (sym, decl) => {
      try {
        const type = decl
          ? checker.getTypeOfSymbolAtLocation(sym, decl)
          : checker.getDeclaredTypeOfSymbol(sym);
        if (!type) return '';
        const text = checker.typeToString(
          type,
          decl,
          ts.TypeFormatFlags.UseAliasDefinedOutsideCurrentScope,
        );
        return text.length > 240 ? `${text.slice(0, 237)}...` : text;
      } catch {
        return '';
      }
    };

    const docFor = (sym, decl, sf) => {
      try {
        const text = ts.displayPartsToString(sym.getDocumentationComment(checker)).trim();
        if (!text) return null;
        let docSpan = null;
        if (decl && sf && sfInfo.has(sf)) {
          const jsDocs = ts.getJSDocCommentsAndTags(decl);
          if (jsDocs && jsDocs.length > 0) {
            docSpan = nodeSpan(sf, jsDocs[0]);
          }
        }
        return { text, source: docSpan };
      } catch {
        return null;
      }
    };

    const linkParent = (childToken, parentToken, parentName, parentKind, parentKey, parentSource) => {
      if (!childToken || !parentToken || childToken === parentToken) return;
      if (parentOf.has(childToken)) return;
      // Cycle guard.
      let cur = parentToken;
      while (cur !== undefined) {
        if (cur === childToken) return;
        cur = parentOf.get(cur);
      }
      parentOf.set(childToken, parentToken);
      const entry = {
        child: childToken,
        parent_token: parentToken,
        key: parentKey,
        name: parentName,
        kind: parentKind,
      };
      if (parentSource) entry.source = parentSource;
      parents.push(entry);
    };

    const ensureSymbol = (rawSym, explicitDecl = null) => {
      const sym = resolveAlias(rawSym);
      if (!sym) return null;
      if (symToToken.has(sym)) return symToToken.get(sym);

      const decl = explicitDecl || pickPrimaryDecl(sym);
      const sf = decl ? decl.getSourceFile() : null;
      const info = sf ? sfInfo.get(sf) : null;
      const unresolvedImport = decl !== null && isImportDecl(decl);
      const external = !info || unresolvedImport;

      let name = sym.getName();
      if (unresolvedImport && ts.isImportSpecifier(decl) && decl.propertyName) {
        name = decl.propertyName.text;
      }
      if (name === 'default' && info && !unresolvedImport) {
        name = path.basename(info.moduleName) || 'default';
      }
      if (!name || name.startsWith('__')) {
        return null;
      }

      // Skip anonymous inline object/type-literal property symbols not owned by a named container.
      if (
        !external &&
        decl &&
        (ts.isPropertySignature(decl) ||
          ts.isPropertyAssignment(decl) ||
          ts.isShorthandPropertyAssignment(decl)) &&
        !isNamedFieldOwner(decl)
      ) {
        return null;
      }

      const kind = unresolvedImport ? 'Symbol' : kindForSymbol(sym, decl);
      const local = !external && isLocalDecl(decl);
      const exported = !external && isNodeExported(decl, sym);
      const qn = unresolvedImport
        ? `${importModuleSpecifier(decl)}::${name}`
        : qualifiedNameFor(sym, decl, name);

      if (external) {
        const extKey = `external::${qn}`;
        if (externalKeyToToken.has(extKey)) {
          const existing = externalKeyToToken.get(extKey);
          symToToken.set(sym, existing);
          return existing;
        }
      }

      let defSpan = null;
      let bodySpan = null;
      if (!external && decl) {
        const nameNode =
          decl.name && (ts.isIdentifier(decl.name) || ts.isStringLiteral(decl.name) || ts.isNumericLiteral(decl.name))
            ? decl.name
            : decl;
        defSpan = nodeSpan(sf, nameNode);
        if (
          ts.isFunctionDeclaration(decl) ||
          ts.isMethodDeclaration(decl) ||
          ts.isGetAccessorDeclaration(decl) ||
          ts.isSetAccessorDeclaration(decl) ||
          ts.isConstructorDeclaration(decl) ||
          ts.isClassDeclaration(decl) ||
          ts.isInterfaceDeclaration(decl) ||
          ts.isTypeAliasDeclaration(decl) ||
          ts.isEnumDeclaration(decl) ||
          ts.isModuleDeclaration(decl) ||
          ts.isVariableDeclaration(decl) ||
          ts.isPropertyDeclaration(decl) ||
          ts.isPropertySignature(decl) ||
          ts.isPropertyAssignment(decl)
        ) {
          bodySpan = nodeSpan(sf, decl);
        }
      }

      const keyBase = external
        ? `external::${qn}`
        : `${qn}@${defSpan ? defSpan.start : 0}`;
      const key = allocateUniqueKey(keyBase);
      const token = nextToken++;

      const rec = {
        token,
        key,
        name,
        qualified_name: qn,
        kind,
        signature: signatureFor(sym, decl),
        external,
        exported,
        local,
        trait_member:
          !external &&
          decl !== null &&
          (ts.isMethodSignature(decl) ||
            (ts.isMethodDeclaration(decl) && hasModifier(decl, ts.SyntaxKind.AbstractKeyword))),
        definition: defSpan,
      };
      if (bodySpan) rec.body = bodySpan;

      if (!external) {
        const doc = docFor(sym, decl, sf);
        if (doc) {
          rec.documentation = doc.text;
          if (doc.source) rec.documentation_source = doc.source;
        }
      } else {
        externalKeyToToken.set(keyBase, token);
      }

      symToToken.set(sym, token);
      tokenRecord.set(token, rec);
      symMeta.set(sym, {
        key,
        name,
        kind,
        fileId: !external && info ? info.id : null,
        defSpan,
        bodySpan,
      });
      if (decl && !external) declNodeToToken.set(decl, token);
      symbols.push(rec);
      return token;
    };

    const collectBindingSpans = (sf, nameNode, out) => {
      if (!nameNode) return;
      if (ts.isIdentifier(nameNode)) {
        const sp = nodeSpan(sf, nameNode);
        if (sp) out.push(sp);
      } else if (ts.isObjectBindingPattern(nameNode) || ts.isArrayBindingPattern(nameNode)) {
        for (const el of nameNode.elements) {
          if (ts.isBindingElement(el)) {
            collectBindingSpans(sf, el.name, out);
          }
        }
      }
    };

    const emitFunctionParameters = (sf, ownerDefSpan, fnParams) => {
      if (!ownerDefSpan || !fnParams || fnParams.length === 0) return;
      const slots = [];
      let index = 0;
      for (const param of fnParams) {
        // Skip TS explicit `this: Foo` fake parameter.
        if (ts.isIdentifier(param.name) && param.name.text === 'this') continue;
        const bindings = [];
        collectBindingSpans(sf, param.name, bindings);
        slots.push({ index, receiver: false, bindings });
        index += 1;
      }
      if (slots.length > 0) {
        parameters.push({ owner: ownerDefSpan, slots });
      }
    };

    const emitFunctionBody = (sf, entityExtentSpan, fnNode, testFlag) => {
      if (!entityExtentSpan || !fnNode || !fnNode.body) return;
      const blockSpan = nodeSpan(sf, fnNode.body);
      if (!blockSpan) return;
      bodies.push({
        kind: 'function',
        source: entityExtentSpan,
        block: blockSpan,
        async: hasModifier(fnNode, ts.SyntaxKind.AsyncKeyword),
        test: testFlag,
      });
    };

    // Pass 1: Declare all project symbols, parent memberships, function bodies,
    // inheritance relations, parameters, and call expressions.
    for (const sf of projectSourceFiles) {
      const info = sfInfo.get(sf);
      const testFile = isTestFile(info.relPath);

      const registerDeclSymbol = (node, nameNode, parentSym = null) => {
        if (nameNode && ts.isIdentifier(nameNode)) {
          emittedDeclNames.add(nameNode);
        }
        const sym = nameNode ? checker.getSymbolAtLocation(nameNode) : checker.getSymbolAtLocation(node);
        const tok = ensureSymbol(sym, node);
        if (tok && parentSym) {
          const pTok = ensureSymbol(parentSym);
          const pMeta = symMeta.get(resolveAlias(parentSym));
          if (pTok && pMeta) {
            linkParent(tok, pTok, pMeta.name, pMeta.kind, pMeta.key, pMeta.defSpan);
          }
        }
        return { sym, tok };
      };

      const visitDecls = (node, enclosingOwnerSym = null) => {
        if (ts.isFunctionDeclaration(node) && node.name) {
          const { sym, tok } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
          if (tok && node.body) {
            const rec = tokenRecord.get(tok);
            emitFunctionBody(sf, rec.body, node, testFile);
            emitFunctionParameters(sf, rec.definition, node.parameters);
          }
          // Register parameters as local symbols.
          for (const param of node.parameters) {
            visitParamDecls(param);
          }
          if (node.body) ts.forEachChild(node.body, (c) => visitDecls(c, null));
          return;
        }

        if (ts.isClassDeclaration(node) && node.name) {
          const { sym } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
          // Heritage clauses (extends / implements).
          if (sym && node.heritageClauses) {
            const fromMeta = symMeta.get(resolveAlias(sym));
            for (const clause of node.heritageClauses) {
              const relKind =
                clause.token === ts.SyntaxKind.ExtendsKeyword ? 'Extends' : 'Implements';
              for (const expr of clause.types) {
                const targetIdent = ts.isPropertyAccessExpression(expr.expression)
                  ? expr.expression.name
                  : expr.expression;
                const targetSym = resolveAlias(checker.getSymbolAtLocation(targetIdent));
                if (targetSym && fromMeta) {
                  ensureSymbol(targetSym);
                  const toMeta = symMeta.get(targetSym);
                  if (toMeta) {
                    const rel = {
                      from: fromMeta.key,
                      to: toMeta.key,
                      file: info.id,
                      kind: relKind,
                    };
                    if (toMeta.fileId !== null) rel.to_file = toMeta.fileId;
                    relations.push(rel);
                  }
                }
              }
            }
          }
          for (const member of node.members) {
            visitClassOrInterfaceMember(member, sym);
          }
          return;
        }

        if (ts.isInterfaceDeclaration(node) && node.name) {
          const { sym } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
          if (sym && node.heritageClauses) {
            const fromMeta = symMeta.get(resolveAlias(sym));
            for (const clause of node.heritageClauses) {
              for (const expr of clause.types) {
                const targetIdent = ts.isPropertyAccessExpression(expr.expression)
                  ? expr.expression.name
                  : expr.expression;
                const targetSym = resolveAlias(checker.getSymbolAtLocation(targetIdent));
                if (targetSym && fromMeta) {
                  ensureSymbol(targetSym);
                  const toMeta = symMeta.get(targetSym);
                  if (toMeta) {
                    const rel = {
                      from: fromMeta.key,
                      to: toMeta.key,
                      file: info.id,
                      kind: 'Extends',
                    };
                    if (toMeta.fileId !== null) rel.to_file = toMeta.fileId;
                    relations.push(rel);
                  }
                }
              }
            }
          }
          for (const member of node.members) {
            visitClassOrInterfaceMember(member, sym);
          }
          return;
        }

        if (ts.isTypeAliasDeclaration(node) && node.name) {
          const { sym } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
          if (sym && node.type && ts.isTypeLiteralNode(node.type)) {
            for (const member of node.type.members) {
              visitClassOrInterfaceMember(member, sym);
            }
            return;
          }
          ts.forEachChild(node, (c) => visitDecls(c, null));
          return;
        }

        if (ts.isEnumDeclaration(node) && node.name) {
          const { sym } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
          for (const member of node.members) {
            registerDeclSymbol(member, member.name, sym);
          }
          return;
        }

        if (ts.isModuleDeclaration(node) && node.name && ts.isIdentifier(node.name)) {
          const { sym } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
          if (node.body) {
            if (ts.isModuleDeclaration(node.body)) visitDecls(node.body, sym);
            else ts.forEachChild(node.body, (c) => visitDecls(c, sym));
          }
          return;
        }

        if (ts.isVariableDeclaration(node)) {
          if (ts.isIdentifier(node.name)) {
            const { sym, tok } = registerDeclSymbol(node, node.name, enclosingOwnerSym);
            const fnInit = unwrapFunctionInitializer(node.initializer);
            if (tok && fnInit) {
              namedFunctionNodes.add(fnInit);
              const rec = tokenRecord.get(tok);
              emitFunctionBody(sf, rec.body, fnInit, testFile);
              emitFunctionParameters(sf, rec.definition, fnInit.parameters);
              for (const param of fnInit.parameters) {
                visitParamDecls(param);
              }
              if (fnInit.body) visitDecls(fnInit.body, null);
              return;
            }
            // Top-level object literal `const draft = { init() {}, ... }`
            const isTopLevel =
              node.parent &&
              ts.isVariableDeclarationList(node.parent) &&
              node.parent.parent &&
              ts.isVariableStatement(node.parent.parent) &&
              ts.isSourceFile(node.parent.parent.parent);
            if (isTopLevel && tok && node.initializer) {
              let initExpr = node.initializer;
              while (ts.isParenthesizedExpression(initExpr) || ts.isAsExpression(initExpr) || ts.isSatisfiesExpression(initExpr)) {
                initExpr = initExpr.expression;
              }
              if (ts.isObjectLiteralExpression(initExpr)) {
                for (const prop of initExpr.properties) {
                  if (ts.isMethodDeclaration(prop) && prop.name && ts.isIdentifier(prop.name)) {
                    const { tok: mTok } = registerDeclSymbol(prop, prop.name, sym);
                    if (mTok && prop.body) {
                      const mRec = tokenRecord.get(mTok);
                      emitFunctionBody(sf, mRec.body, prop, testFile);
                      emitFunctionParameters(sf, mRec.definition, prop.parameters);
                      for (const param of prop.parameters) visitParamDecls(param);
                      ts.forEachChild(prop.body, (c) => visitDecls(c, null));
                    }
                  } else if (ts.isPropertyAssignment(prop) && ts.isIdentifier(prop.name)) {
                    const pFn = unwrapFunctionInitializer(prop.initializer);
                    const { tok: pTok } = registerDeclSymbol(prop, prop.name, sym);
                    if (pTok && pFn) {
                      namedFunctionNodes.add(pFn);
                      const pRec = tokenRecord.get(pTok);
                      emitFunctionBody(sf, pRec.body, pFn, testFile);
                      emitFunctionParameters(sf, pRec.definition, pFn.parameters);
                      for (const param of pFn.parameters) visitParamDecls(param);
                      if (pFn.body) visitDecls(pFn.body, null);
                    } else {
                      visitDecls(prop.initializer, null);
                    }
                  } else {
                    visitDecls(prop, null);
                  }
                }
                return;
              }
              // Non-function top-level initializer (e.g. `const X = lazy(() => ...)`)
              const rec = tokenRecord.get(tok);
              const initSpan = nodeSpan(sf, node.initializer);
              if (rec && rec.definition && initSpan) {
                bodies.push({
                  kind: 'const_initializer',
                  source: initSpan,
                  owner: rec.definition,
                });
              }
            }
          } else {
            visitBindingDecls(node.name);
          }
          if (node.initializer) visitDecls(node.initializer, null);
          return;
        }

        // Anonymous arrow / function expressions -> Closure bodies.
        if (
          (ts.isArrowFunction(node) || ts.isFunctionExpression(node)) &&
          !namedFunctionNodes.has(node)
        ) {
          const sp = nodeSpan(sf, node);
          if (sp) {
            projectClosures.push({
              kind: 'closure',
              source: sp,
              size: sp.end - sp.start,
            });
          }
          for (const param of node.parameters) {
            visitParamDecls(param);
          }
          if (node.body) visitDecls(node.body, null);
          return;
        }

        // Record CallExpression / NewExpression / JSX component calls.
        if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
          let calleeNode = node.expression;
          while (ts.isParenthesizedExpression(calleeNode)) calleeNode = calleeNode.expression;
          let method = false;
          let calleeIdent = null;
          if (ts.isPropertyAccessExpression(calleeNode)) {
            method = true;
            calleeIdent = calleeNode.name;
          } else if (ts.isIdentifier(calleeNode)) {
            calleeIdent = calleeNode;
          }
          if (calleeIdent) {
            const calleeSpan = nodeSpan(sf, calleeIdent);
            const callSpan = nodeSpan(sf, node);
            if (calleeSpan && callSpan) {
              const callArgs = (node.arguments || [])
                .map((a) => nodeSpan(sf, a))
                .filter(Boolean);
              const idx = calls.length;
              calls.push({
                callee: calleeSpan,
                source: callSpan,
                arguments: callArgs,
                method,
              });
              callCalleeMap.set(calleeIdent, idx);
              if (ts.isNewExpression(node)) newCalleeSet.add(calleeIdent);
            }
          }
        } else if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
          const tag = node.tagName;
          let tagIdent = null;
          if (ts.isIdentifier(tag) && /^[A-Z]/.test(tag.text)) {
            tagIdent = tag;
          } else if (ts.isPropertyAccessExpression(tag)) {
            tagIdent = tag.name;
          }
          if (tagIdent) {
            const calleeSpan = nodeSpan(sf, tagIdent);
            const jsxSpan = nodeSpan(sf, node);
            if (calleeSpan && jsxSpan) {
              const idx = calls.length;
              calls.push({
                callee: calleeSpan,
                source: jsxSpan,
                arguments: [],
                method: false,
              });
              callCalleeMap.set(tagIdent, idx);
            }
          }
        }

        ts.forEachChild(node, (c) => visitDecls(c, null));
      };

      const visitBindingDecls = (nameNode) => {
        if (!nameNode) return;
        if (ts.isIdentifier(nameNode)) {
          emittedDeclNames.add(nameNode);
          const sym = checker.getSymbolAtLocation(nameNode);
          ensureSymbol(sym, nameNode.parent);
        } else if (ts.isObjectBindingPattern(nameNode) || ts.isArrayBindingPattern(nameNode)) {
          for (const el of nameNode.elements) {
            if (ts.isBindingElement(el)) {
              visitBindingDecls(el.name);
            }
          }
        }
      };

      const visitParamDecls = (param) => {
        if (ts.isIdentifier(param.name)) {
          if (param.name.text === 'this') return;
          emittedDeclNames.add(param.name);
          const sym = checker.getSymbolAtLocation(param.name);
          ensureSymbol(sym, param);
        } else {
          visitBindingDecls(param.name);
        }
      };

      const visitClassOrInterfaceMember = (member, ownerSym) => {
        if (
          ts.isMethodDeclaration(member) ||
          ts.isGetAccessorDeclaration(member) ||
          ts.isSetAccessorDeclaration(member)
        ) {
          if (member.name && ts.isIdentifier(member.name)) {
            const { tok } = registerDeclSymbol(member, member.name, ownerSym);
            if (tok && member.body) {
              const rec = tokenRecord.get(tok);
              emitFunctionBody(sf, rec.body, member, testFile);
              emitFunctionParameters(sf, rec.definition, member.parameters);
            }
          }
          for (const p of member.parameters) visitParamDecls(p);
          if (member.body) ts.forEachChild(member.body, (c) => visitDecls(c, null));
        } else if (ts.isConstructorDeclaration(member)) {
          for (const p of member.parameters) visitParamDecls(p);
          if (member.body) ts.forEachChild(member.body, (c) => visitDecls(c, null));
        } else if (
          ts.isMethodSignature(member) ||
          ts.isPropertyDeclaration(member) ||
          ts.isPropertySignature(member)
        ) {
          if (member.name && ts.isIdentifier(member.name)) {
            registerDeclSymbol(member, member.name, ownerSym);
          }
          if (ts.isPropertyDeclaration(member) && member.initializer) {
            visitDecls(member.initializer, null);
          }
        }
      };

      visitDecls(sf, null);
    }

    // Sort closures outer-to-inner so nested closures attach to their enclosing body cleanly.
    projectClosures.sort((a, b) => b.size - a.size);
    for (const c of projectClosures) {
      bodies.push({ kind: c.kind, source: c.source });
    }

    // Populate type_definitions on project symbols whose declared/return type is a project type.
    const collectNamedTypeDefs = (type, out, seen, depth) => {
      if (!type || depth > 3 || seen.has(type)) return;
      seen.add(type);
      if (type.aliasSymbol) {
        const s = resolveAlias(type.aliasSymbol);
        const meta = symMeta.get(s);
        if (meta && meta.defSpan) {
          out.push(meta);
          return;
        }
      }
      if (type.symbol) {
        const s = resolveAlias(type.symbol);
        const meta = symMeta.get(s);
        if (meta && meta.defSpan) {
          out.push(meta);
          return;
        }
      }
      if (type.isUnionOrIntersection && type.isUnionOrIntersection()) {
        for (const t of type.types || []) {
          collectNamedTypeDefs(t, out, seen, depth + 1);
        }
      } else if (checker.getTypeArguments) {
        try {
          const typeArgs = checker.getTypeArguments(type);
          if (typeArgs && typeArgs.length > 0 && typeArgs.length <= 4) {
            for (const arg of typeArgs) {
              collectNamedTypeDefs(arg, out, seen, depth + 1);
            }
          }
        } catch {
          // ignore
        }
      }
    };

    for (const [sym, tok] of symToToken.entries()) {
      const rec = tokenRecord.get(tok);
      if (!rec || rec.external) continue;
      const decl = pickPrimaryDecl(sym);
      if (!decl) continue;
      try {
        let typ = checker.getTypeOfSymbolAtLocation(sym, decl);
        if (typ) {
          const sigs = typ.getCallSignatures ? typ.getCallSignatures() : [];
          if (sigs.length === 1 && (rec.kind === 'Function' || rec.kind === 'Method')) {
            typ = sigs[0].getReturnType();
          }
          const targets = [];
          collectNamedTypeDefs(typ, targets, new Set(), 0);
          const unique = [];
          const seenKeys = new Set([rec.key]);
          for (const meta of targets) {
            if (!seenKeys.has(meta.key)) {
              seenKeys.add(meta.key);
              const nav = { source: meta.defSpan };
              if (meta.bodySpan) nav.body = meta.bodySpan;
              unique.push(nav);
            }
          }
          if (unique.length > 0) {
            rec.type_definitions = unique;
          }
        }
      } catch {
        // ignore type resolution failure
      }
    }

    // Pass 2: Emit all semantic references (imports, exports, calls, JSX, types, accesses, reads).
    const isInsideTypePosition = (node) => {
      let cur = node.parent;
      while (cur) {
        if (
          ts.isTypeReferenceNode(cur) ||
          ts.isExpressionWithTypeArguments(cur) ||
          ts.isTypeQueryNode(cur)
        ) {
          return true;
        }
        if (
          ts.isQualifiedName(cur) ||
          ts.isPropertyAccessExpression(cur)
        ) {
          cur = cur.parent;
          continue;
        }
        break;
      }
      return false;
    };

    const isPropertyNameLabel = (ident) => {
      const p = ident.parent;
      if (!p) return false;
      if (ts.isPropertyAssignment(p) && p.name === ident) return true;
      if (ts.isBindingElement(p) && p.propertyName === ident) return true;
      if (ts.isJsxAttribute(p) && p.name === ident) return true;
      if (ts.isJsxClosingElement(p)) return true;
      if (ts.isImportSpecifier(p) && p.propertyName === ident) return true;
      if (ts.isExportSpecifier(p) && p.propertyName === ident) return true;
      return false;
    };

    for (const sf of projectSourceFiles) {
      const emitRef = (ident, targetSym, kind, callIdx = null) => {
        const resolved = resolveAlias(targetSym);
        if (!resolved) return;
        const tok = ensureSymbol(resolved);
        if (!tok) return;
        const sp = nodeSpan(sf, ident);
        if (!sp) return;
        const refObj = {
          target: tok,
          source: sp,
          kind,
          is_definition: false,
        };
        if (callIdx !== null) refObj.call_index = callIdx;
        references.push(refObj);
      };

      const visitRefs = (node) => {
        // Skip JSX closing tags to avoid duplicate references.
        if (ts.isJsxClosingElement(node)) return;

        // Import specifiers: `import { a, b as c } from './mod'`
        if (ts.isImportSpecifier(node)) {
          const sym = checker.getSymbolAtLocation(node.name);
          const resolved = resolveAlias(sym);
          if (resolved && resolved !== sym) {
            emitRef(node.name, resolved, 'Import');
          }
          return;
        }
        if (ts.isImportClause(node) && node.name) {
          const sym = checker.getSymbolAtLocation(node.name);
          const resolved = resolveAlias(sym);
          if (resolved && resolved !== sym) {
            emitRef(node.name, resolved, 'Import');
          }
          if (node.namedBindings) visitRefs(node.namedBindings);
          return;
        }
        if (ts.isExportSpecifier(node)) {
          const sym = checker.getSymbolAtLocation(node.name);
          const resolved = resolveAlias(sym);
          if (resolved) {
            emitRef(node.name, resolved, 'Export');
          }
          return;
        }

        // Shorthand property assignment `{ queryKey }` references local variable `queryKey`.
        if (ts.isShorthandPropertyAssignment(node)) {
          const valSym = checker.getShorthandAssignmentValueSymbol(node);
          if (valSym) {
            emitRef(node.name, valSym, 'Reference');
          }
          return;
        }

        if (ts.isIdentifier(node)) {
          if (emittedDeclNames.has(node) || isPropertyNameLabel(node)) return;
          // Skip lowercase intrinsic HTML/SVG JSX tag names like `<div>`.
          if (
            (ts.isJsxOpeningElement(node.parent) || ts.isJsxSelfClosingElement(node.parent)) &&
            node.parent.tagName === node &&
            !/^[A-Z]/.test(node.text)
          ) {
            return;
          }
          const sym = checker.getSymbolAtLocation(node);
          if (!sym) return;

          const callIdx = callCalleeMap.has(node) ? callCalleeMap.get(node) : null;
          let kind = 'Reference';
          if (callIdx !== null) {
            kind = newCalleeSet.has(node) ? 'Instantiate' : 'Call';
          } else if (isInsideTypePosition(node)) {
            kind = 'Type';
          } else if (
            ts.isPropertyAccessExpression(node.parent) &&
            node.parent.name === node
          ) {
            kind = 'Access';
          }
          emitRef(node, sym, kind, callIdx);
          return;
        }

        ts.forEachChild(node, visitRefs);
      };

      visitRefs(sf);
    }
  }

  const output = {
    protocol: 'codequest-hir-export',
    schema_version: 1,
    provider: 'ts-compiler',
    version: ts.version,
    files: rawFiles,
    symbols,
    references,
    parents,
    relations,
    bodies,
    calls,
    doc_links: [],
    comments,
    parameters,
    diagnostics,
  };

  fs.writeFileSync(outputPath, JSON.stringify(output));
}

run();

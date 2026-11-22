//! A bounded forest scheduler splits large directory trees into leaf jobs.
//! Work spills instead of growing with the complete graph. Every member is
//! checked against the independent payload-derived proof, including its path.
use super::*;
use cq_core::ports::facts::ObjectRef;
use std::io::{BufReader, BufWriter};
const MAX_PENDING: usize = 16 * 1024;
const MAX_JOBS: usize = 4096;
const READ_BYTES: u64 = 8 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct Work {
    group: usize,
    object: ObjectRef,
    prefix: String,
}
#[derive(Default, serde::Serialize)]
pub(super) struct DirectoryStats {
    pub leaf_jobs: u64,
    pub branch_jobs: u64,
    pub member_checks: u64,
    pub maximum_pending_jobs: usize,
    pub spilled_waves: u64,
    pub io: crate::facts::objects::BatchReadStats,
}
impl DirectoryStats {
    pub fn add(&mut self, b: Self) {
        self.leaf_jobs += b.leaf_jobs;
        self.branch_jobs += b.branch_jobs;
        self.member_checks += b.member_checks;
        self.maximum_pending_jobs = self.maximum_pending_jobs.max(b.maximum_pending_jobs);
        self.spilled_waves += b.spilled_waves;
        self.io.add(b.io);
    }
}
impl Proof {
    pub(super) fn directories(
        &mut self,
        objects: &Objects,
        wave: Vec<(String, writer::Group)>,
        progress: &mut cq_core::progress::Progress,
        completed: &mut u64,
    ) -> Result<DirectoryStats> {
        self.directories_controlled(objects, wave, progress, completed, None)
    }
    pub(super) fn directories_controlled(
        &mut self,
        objects: &Objects,
        wave: Vec<(String, writer::Group)>,
        progress: &mut cq_core::progress::Progress,
        completed: &mut u64,
        control: Option<&cq_core::control::CaptureControl>,
    ) -> Result<DirectoryStats> {
        let mut ids = Vec::with_capacity(wave.len());
        let mut tasks = Vec::new();
        let mut expected = Vec::with_capacity(wave.len());
        for (i, (name, group)) in wave.into_iter().enumerate() {
            let id = *self
                .group_ids
                .get(&hash(name.as_bytes()))
                .ok_or_else(|| Error::new("IndexIncomplete", "foreign directory"))?;
            let proof = &mut self.groups[id as usize];
            if proof.seen {
                return Err(Error::new("IndexIncomplete", "duplicate directory"));
            }
            proof.seen = true;
            ids.push(id);
            expected.push(group.count);
            if let Some(object) = group.root {
                tasks.push(Work {
                    group: i,
                    object,
                    prefix: String::new(),
                });
            }
        }
        let mut counts = vec![0u64; ids.len()];
        let mut stats = DirectoryStats::default();
        let base_completed = *completed;
        let mut scratch = None;
        let mut spills = Vec::new();
        while !tasks.is_empty() || !spills.is_empty() {
            if control.is_some_and(|c| c.is_cancelled()) {
                return Err(Error::new("Cancelled", "audit directory wave paused"));
            }
            if tasks.is_empty() {
                let path = spills.pop().unwrap();
                tasks = serde_json::from_reader(BufReader::new(std::fs::File::open(&path)?))?;
                std::fs::remove_file(path)?;
            }
            stats.maximum_pending_jobs = stats.maximum_pending_jobs.max(tasks.len());
            let mut jobs = Vec::new();
            let mut bytes = 0;
            while let Some(task) = tasks.pop() {
                if !jobs.is_empty()
                    && (bytes + task.object.bytes > READ_BYTES || jobs.len() == MAX_JOBS)
                {
                    tasks.push(task);
                    break;
                }
                bytes += task.object.bytes;
                jobs.push(task);
            }
            let refs = jobs.iter().map(|j| j.object.clone()).collect::<Vec<_>>();
            let (nodes, io) =
                objects.audit_batch(&refs, |_, b| Ok(serde_json::from_slice::<tree::Node>(b)?))?;
            stats.io.add(io);
            // Member checks are CPU work, separate from the physical reader.
            let checked =
                cq_core::compute::map(jobs.into_iter().zip(nodes).collect(), |(job, node)| {
                    match node {
                        tree::Node::Leaf(rows) => {
                            let mut count = 0;
                            for (sort, value) in rows {
                                let key = sort
                                    .get(65..)
                                    .ok_or_else(|| Error::new("InvalidManifestTree", "leaf key"))?;
                                if !sort.starts_with(&job.prefix) || tree::order(key) != sort {
                                    return Err(Error::new(
                                        "InvalidManifestTree",
                                        "misplaced directory member",
                                    ));
                                }
                                let entry =
                                    self.facts.get(&hash(key.as_bytes())).ok_or_else(|| {
                                        Error::new("IndexIncomplete", "foreign directory member")
                                    })?;
                                if entry.reference != reference_hash(&value)?
                                    || !self.group_members[entry.groups.clone()]
                                        .contains(&ids[job.group])
                                {
                                    return Err(Error::new(
                                        "IndexIncomplete",
                                        "directory owner/reference mismatch",
                                    ));
                                }
                                count += 1;
                            }
                            Ok((job.group, count, Vec::new(), true))
                        }
                        tree::Node::Branch(children) => {
                            if children.is_empty()
                                || job.prefix.len() >= 64
                                || children.keys().any(|n| *n > 15)
                            {
                                return Err(Error::new(
                                    "InvalidManifestTree",
                                    "invalid directory branch",
                                ));
                            }
                            let children = children
                                .into_iter()
                                .map(|(n, object)| Work {
                                    group: job.group,
                                    object,
                                    prefix: format!("{}{n:x}", job.prefix),
                                })
                                .collect();
                            Ok((job.group, 0, children, false))
                        }
                    }
                })?;
            for (group, count, children, leaf) in checked {
                counts[group] += count;
                stats.member_checks += count;
                stats.leaf_jobs += u64::from(leaf);
                stats.branch_jobs += u64::from(!leaf);
                tasks.extend(children);
            }
            // Count this wave, not configured workers or completed directories.
            *completed = base_completed + stats.member_checks;
            progress.count(*completed);
            while tasks.len() > MAX_PENDING {
                if scratch.is_none() {
                    scratch = Some(tempfile::tempdir()?);
                }
                let spill = tasks.split_off(tasks.len() - MAX_PENDING);
                let path = scratch
                    .as_ref()
                    .unwrap()
                    .path()
                    .join(format!("{}.json", stats.spilled_waves));
                let mut out = BufWriter::new(std::fs::File::create(&path)?);
                serde_json::to_writer(&mut out, &spill)?;
                std::io::Write::flush(&mut out)?;
                spills.push(path);
                stats.spilled_waves += 1;
            }
        }
        for (i, id) in ids.into_iter().enumerate() {
            if counts[i] != expected[i] || counts[i] != self.groups[id as usize].expected {
                return Err(Error::new("IndexIncomplete", "directory count mismatch"));
            }
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn many_large_directory_roots_spill_without_losing_or_duplicating_members() {
        let t = tempfile::tempdir().unwrap();
        let o = Objects::buffered(t.path().join("objects")).unwrap();
        let rows = (0..64)
            .map(|i| {
                let key = format!("entity:{i}");
                (tree::order(&key), json!({"ref":i}))
            })
            .collect::<BTreeMap<_, _>>();
        let (root, count) = tree::build_sorted(&o, rows.clone().into_iter().map(Ok)).unwrap();
        o.sync_pending().unwrap();
        let o = Objects::new(o.root.clone()).unwrap();
        let groups = 4096u32;
        let mut proof = Proof::default();
        let mut wave = Vec::new();
        for id in 0..groups {
            let name = format!("test:{id}");
            proof.group_ids.insert(hash(name.as_bytes()), id);
            proof.groups.push(Group {
                expected: count,
                seen: false,
            });
            wave.push((
                name,
                writer::Group {
                    root: root.clone(),
                    count,
                },
            ));
        }
        for (sort, value) in &rows {
            let start = proof.group_members.len();
            proof.group_members.extend(0..groups);
            proof.facts.insert(
                hash(&sort.as_bytes()[65..]),
                Fact {
                    reference: reference_hash(value).unwrap(),
                    groups: start..proof.group_members.len(),
                    family: None,
                    owner: Owner::Project,
                    scoped: false,
                },
            );
        }
        let mut progress = cq_core::progress::Progress::stage(
            t.path(),
            "spill-test",
            "audit.directories",
            "审计目录",
            Some(groups as u64 * count),
            "成员",
        );
        let mut completed = 0;
        let stats = proof
            .directories(&o, wave, &mut progress, &mut completed)
            .unwrap();
        assert!(stats.spilled_waves > 0);
        assert!(stats.maximum_pending_jobs <= MAX_PENDING);
        assert_eq!(completed, groups as u64 * count);
        assert_eq!(stats.member_checks, completed);
        assert!(proof.groups.iter().all(|g| g.seen));
    }
}

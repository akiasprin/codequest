//! Large collections of reclaim candidates use an immutable reviewed plan file;
//! the IPC request and recovery journal contain only its bound reference.
use super::retention::RetentionPlan;
use super::*;
use std::io::{Read, Seek, SeekFrom};
const INLINE_OBJECTS: usize = 65536;
const MAX_PLAN: u64 = 256 * 1024 * 1024;
#[derive(Serialize)]
struct Envelope<'a> {
    project: &'a str,
    plan: &'a RetentionPlan,
}
impl StoreHost {
    fn store_plan(&self, plan: &RetentionPlan) -> Result<Value> {
        let dir = self.root.join("retention-plans");
        crate::atomic::create_directory(&dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(
            tmp.as_file_mut(),
            &Envelope {
                project: &self.project,
                plan,
            },
        )?;
        let bytes = tmp.as_file().metadata()?.len();
        if bytes > MAX_PLAN {
            return Err(Error::new(
                "RetentionPlanBudget",
                "review plan exceeds 256 MiB",
            ));
        }
        tmp.as_file_mut().seek(SeekFrom::Start(0))?;
        let hash = plan_hash(tmp.as_file_mut())?;
        tmp.as_file().sync_all()?;
        let path = dir.join(format!("{hash}.json"));
        match tmp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if plan_hash(&mut File::open(&path)?)? != hash {
                    return Err(Error::new("DigestMismatch", "retention plan"));
                }
            }
            Err(e) => return Err(e.error.into()),
        };
        File::open(&dir)?.sync_all()?;
        File::open(&self.root)?.sync_all()?;
        Ok(
            json!({"plan_file":hash,"project":self.project,"path":path,"generation":plan.generation,"revisions":plan.revisions,"object_count":plan.objects.len(),"bytes":plan.bytes,"review_required":true}),
        )
    }
    pub fn plan_response(&self, plan: &RetentionPlan) -> Result<Value> {
        if plan.objects.len() > INLINE_OBJECTS {
            self.store_plan(plan)
        } else {
            Ok(serde_json::to_value(plan)?)
        }
    }
    pub fn read_plan_reference(&self, id: &str) -> Result<RetentionPlan> {
        if !valid_hash(id) {
            return Err(Error::new("InvalidRetentionPlan", "plan handle"));
        }
        let path = self.root.join("retention-plans").join(format!("{id}.json"));
        let mut file = File::open(path)?;
        if file.metadata()?.len() > MAX_PLAN {
            return Err(Error::new("RetentionPlanBudget", "plan file size"));
        }
        if plan_hash(&mut file)? != id {
            return Err(Error::new("DigestMismatch", "retention plan"));
        }
        file.seek(SeekFrom::Start(0))?;
        let v: Value = serde_json::from_reader(file)?;
        if v["project"] != self.project {
            return Err(Error::new("WrongProject", "retention plan"));
        }
        Ok(serde_json::from_value(v["plan"].clone())?)
    }
    pub(super) fn save_retirement(&self, plan: &RetentionPlan) -> Result<()> {
        let path = self.root.join("retirement.json");
        if plan.objects.len() > INLINE_OBJECTS {
            save(&path, &self.store_plan(plan)?)
        } else {
            save(&path, plan)
        }
    }
    pub(super) fn load_retirement(&self) -> Result<RetentionPlan> {
        let v: Value = load(&self.root.join("retirement.json"))?;
        if let Some(id) = v["plan_file"].as_str() {
            self.read_plan_reference(id)
        } else {
            Ok(serde_json::from_value(v)?)
        }
    }
}
fn plan_hash(file: &mut File) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut b = [0; 64 * 1024];
    loop {
        let n = file.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

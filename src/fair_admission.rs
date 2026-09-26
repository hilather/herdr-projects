//! Bounded fairness history, independent of candidate refresh and eviction.
use std::collections::{BTreeMap,BTreeSet};
pub type Key=(String,String);
const LIMIT:usize=128;
#[derive(Default,Clone)]
pub struct Cursor {last_project:Option<String>,threads:BTreeMap<String,String>,last_pair:Option<Key>,overflow:bool}
impl Cursor {
    pub fn compare(&self,a:&Key,b:&Key)->std::cmp::Ordering {
        if self.overflow {return (self.last_pair.as_ref().is_some_and(|last|a<=last),a).cmp(&(self.last_pair.as_ref().is_some_and(|last|b<=last),b));}
        let rank=|key:&Key|(self.last_project.as_ref().is_some_and(|last|&key.0<=last),self.threads.get(&key.0).is_some_and(|last|&key.1<=last));
        let ar=rank(a);let br=rank(b);(ar.0,&a.0,ar.1,&a.1).cmp(&(br.0,&b.0,br.1,&b.1))
    }
    pub fn accepted(&mut self,key:&Key) {
        if !self.overflow&&!self.threads.contains_key(&key.0)&&self.threads.len()>=LIMIT {self.overflow=true;self.threads.clear();}
        if !self.overflow {self.threads.insert(key.0.clone(),key.1.clone());}
        self.last_project=Some(key.0.clone());self.last_pair=Some(key.clone());
    }
    #[cfg(test)]
    pub fn history_len(&self)->usize {self.threads.len()}
    #[cfg(test)]
    pub fn overflow(&self)->bool {self.overflow}
}
/// Cancel, reconcile, and observation stay eligible on Control when Transfer is full.
pub fn reserved_control(operation:&str)->bool {
    operation.starts_with("canonical-cancel:")||operation.starts_with("reconcile:")||operation.starts_with("canonical-observation")||operation.starts_with("local-observation")
}
/// One service per project per round. A project already served waits until every other waiting project has had a turn.
#[derive(Default)]
pub struct ProjectRound {served:BTreeSet<String>}
impl ProjectRound {
    pub fn select(&mut self,eligible:&[(usize,String)])->Option<usize> {
        if eligible.is_empty(){return None;}
        if eligible.iter().all(|(_,project)|self.served.contains(project)){self.served.clear();}
        let (index,project)=eligible.iter().filter(|(_,project)|!self.served.contains(project)).min_by_key(|(index,_)|*index)?;
        self.served.insert(project.clone());Some(*index)
    }
}
#[cfg(test)]
mod tests {
    use super::{ProjectRound,reserved_control};
    #[test]
    fn reserved_control_is_cancel_reconcile_and_observation() {
        assert!(reserved_control("canonical-cancel:1"));assert!(reserved_control("reconcile:receipt"));
        assert!(reserved_control("canonical-observation"));assert!(reserved_control("local-observation"));
        assert!(!reserved_control("canonical-launch:1"));assert!(!reserved_control("cancel-transfer"));
    }
    #[test]
    fn eight_projects_each_start_within_two_fairness_rounds() {
        let mut jobs=Vec::new();
        for project in 0..8 {for _ in 0..4 {jobs.push(format!("p{project}"));}}
        let mut round=ProjectRound::default();let mut started=Vec::new();
        for _ in 0..16 {
            let eligible:Vec<(usize,String)>=jobs.iter().enumerate().map(|(index,project)|(index,project.clone())).collect();
            let index=round.select(&eligible).unwrap();started.push(jobs.remove(index));
        }
        let mut seen:std::collections::BTreeSet<_>=started.iter().take(8).cloned().collect();
        assert_eq!(seen.len(),8,"first round missed a project: {started:?}");
        seen.clear();seen.extend(started.iter().cloned());assert_eq!(seen.len(),8,"two rounds dropped a project: {started:?}");
    }
}

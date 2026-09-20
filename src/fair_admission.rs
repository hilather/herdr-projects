//! Bounded fairness history, independent of candidate refresh and eviction.
use std::collections::BTreeMap;
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

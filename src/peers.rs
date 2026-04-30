use crate::types::Peer;
use dashmap::DashMap;
use std::collections::VecDeque;

const MAX_INFOHASHES: usize = 50000;

pub struct PeersManager {
    table: DashMap<[u8;20], VecDeque<Peer>>,
    k: usize,
}

impl PeersManager {
    pub fn new(k: usize) -> Self { Self{ table: DashMap::new(), k } }
    pub fn insert(&self, ih: [u8;20], peer: Peer) {
        if self.table.len() >= MAX_INFOHASHES && !self.table.contains_key(&ih) { return; }
        let mut entry=self.table.entry(ih).or_insert_with(VecDeque::new); if let Some(pos)=entry.iter().position(|p| p.ip==peer.ip && p.port==peer.port){ entry.remove(pos); } entry.push_back(peer); while entry.len()>self.k { entry.pop_front(); }
    }
    pub fn get(&self, ih: [u8;20], size: usize) -> Vec<Peer> { self.table.get(&ih).map(|v| { let len=v.len(); let start=len.saturating_sub(size); v.iter().skip(start).cloned().collect() }).unwrap_or_default() }
}

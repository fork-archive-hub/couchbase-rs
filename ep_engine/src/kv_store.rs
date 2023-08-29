use crate::vbucket::{VBucketState, Vbid};
use couchstore::Db;
use parking_lot::RwLock;
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub struct CouchKVStoreConfig {
    max_vbuckets: u16,
    db_name: String,
    max_shards: u16,
    shard_id: u16,
}

impl CouchKVStoreConfig {
    fn get_cache_size(&self) -> usize {
        (self.max_vbuckets as f64 / self.max_shards as f64).ceil() as usize
    }
}

type RevisionMap = RwLock<Vec<u64>>;

pub struct CouchKVStore {
    config: CouchKVStoreConfig,
    db_file_rev_map: Arc<RevisionMap>,
    cached_vb_states: Vec<Option<VBucketState>>,
}

impl CouchKVStore {
    pub fn new(config: CouchKVStoreConfig) -> Self {
        let mut store = Self {
            db_file_rev_map: make_revision_map(&config),
            config,
            cached_vb_states: Vec::new(),
        };

        let cache_size = store.config.get_cache_size();

        store.cached_vb_states.resize(cache_size, None);

        // 1) populate the dbFileRevMap which can remove old revisions, this returns
        //    a map, which the keys (vbid) will be needed for step 3 and 4.
        let map = store.populate_rev_map_and_remove_stale_files();

        // 2) clean up any .compact files
        for &vbid in map.keys() {
            store.maybe_remove_compact_file(vbid);
        }

        // 3) continue to intialise the store (reads vbstate etc...)
        store.initialise(map);

        store
    }

    fn initialise(&mut self, map: HashMap<Vbid, HashSet<u64>>) {
        for &vbid in map.keys() {
            let mut options = couchstore::DBOpenOptions::default();
            options.read_only = true;

            let mut db = self.open_db(vbid, options);

            self.read_vb_state_and_update_cache(&mut db, vbid);
        }
    }

    fn read_vb_state_and_update_cache(&mut self, db: &mut Db, vbid: Vbid) -> &VBucketState {
        let vb_state = self.read_vb_state(db, vbid);

        let slot = self.get_cache_slot(vbid);
        self.cached_vb_states[slot] = Some(vb_state);

        self.cached_vb_states[slot].as_ref().unwrap()
    }

    fn populate_rev_map_and_remove_stale_files(&self) -> HashMap<Vbid, HashSet<u64>> {
        let map = self.get_vbucket_revision(discover_db_files(&self.config.db_name));

        for (&vbid, revs) in &map {
            for &revision in revs {
                let mut current = self.get_db_revision(vbid);
                match current.cmp(&revision) {
                    Ordering::Equal => {
                        continue;
                    }
                    Ordering::Less => {
                        // current file is stale, update to the new revision
                        self.update_db_file_map(vbid, revision);
                    }
                    Ordering::Greater => {
                        // stale file found (revision id has rolled over)
                        current = revision
                    }
                }

                // stale file left behind to be removed
                let stale_file = get_db_file_name(&self.config.db_name, vbid, current);

                if std::fs::metadata(&stale_file).is_ok() {
                    std::fs::remove_file(&stale_file).unwrap();
                    println!("Removed stale file {}", stale_file);
                }
            }
        }

        map
    }

    fn get_db_revision(&self, vbid: Vbid) -> u64 {
        let map = self.db_file_rev_map.read();
        map[self.get_cache_slot(vbid)]
    }

    fn get_cache_slot(&self, vbid: Vbid) -> usize {
        (vbid.0 / self.config.max_shards) as usize
    }

    fn get_vbucket_revision(&self, filenames: Vec<String>) -> HashMap<Vbid, HashSet<u64>> {
        let mut vbids = HashMap::new();
        for filename in filenames {
            let parts: Vec<&str> = filename.split('.').collect();
            assert_eq!(parts.len(), 3);
            // master.couch.x is expected and can be silently ignored
            if parts[0] == "master" {
                continue;
            }
            // TODO: Error handling
            let vbid = Vbid::new(parts[0].parse().unwrap());
            let rev = parts[2].parse().unwrap();

            if vbid % self.config.max_shards != self.config.shard_id {
                // Either doesn't belong to this shard or is the last element
                // (case where max vB % shards != 0) which we now need to check
                // for
                if vbid.0
                    != (((self.config.max_vbuckets / self.config.max_shards)
                        * self.config.max_shards)
                        + self.config.shard_id)
                {
                    continue;
                }
            }

            vbids.entry(vbid).or_insert_with(HashSet::new).insert(rev);
        }
        vbids
    }

    fn update_db_file_map(&self, vbid: Vbid, revision: u64) {
        let mut map = self.db_file_rev_map.write();
        map[self.get_cache_slot(vbid)] = revision;
    }

    fn maybe_remove_compact_file(&self, vbid: Vbid) {
        let revision = self.get_db_revision(vbid);
        let compact_file = get_db_file_name(&self.config.db_name, vbid, revision) + ".compact";
        if std::fs::metadata(&compact_file).is_ok() {
            std::fs::remove_file(&compact_file).unwrap();
            println!("Removed compact file {}", compact_file);
        }
    }

    fn open_db(&self, vbid: Vbid, options: couchstore::DBOpenOptions) -> couchstore::Db {
        let rev_map = self.db_file_rev_map.read();
        let file_rev = rev_map[self.get_cache_slot(vbid)];
        let file_name = get_db_file_name(&self.config.db_name, vbid, file_rev);
        self.open_specific_db_file(vbid, file_rev, options, file_name)
    }

    fn open_specific_db_file(
        &self,
        _vbid: Vbid,
        _file_rev: u64,
        options: couchstore::DBOpenOptions,
        file_name: String,
    ) -> Db {
        // TODO: args used for loggin
        Db::open(file_name, options)
    }

    fn read_vb_state(&self, db: &mut Db, _vbid: Vbid) -> VBucketState {
        let header = self.read_header(db);
        let high_seqno = header.update_seq as i64;
        let purge_seqno = header.purge_seq;

        let vb_state = get_local_vb_state(db);

        let mut vb_state: VBucketState = serde_json::from_value(vb_state).unwrap();

        vb_state.high_seqno = high_seqno;
        vb_state.purge_seqno = purge_seqno;

        // MB-17517: If the maxCas on disk was invalid then don't use it -
        // instead rebuild from the items we load from disk (i.e. as per
        // an upgrade from an earlier version).
        if vb_state.max_cas == std::u64::MAX {
            vb_state.max_cas = 0;
        }

        vb_state
    }

    fn read_header<'a>(&self, db: &'a Db) -> &'a couchstore::Header {
        db.header()
    }
}

fn discover_db_files(dir: &str) -> Vec<String> {
    let mut filenames = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let file_name = entry.file_name();
        let file_name = file_name.to_str().unwrap();
        if file_name.contains(".couch.") && !file_name.ends_with(".compact") {
            filenames.push(file_name.to_string());
        }
    }
    filenames
}

fn make_revision_map(config: &CouchKVStoreConfig) -> Arc<RevisionMap> {
    let map = Arc::new(RevisionMap::default());
    map.write().resize(config.get_cache_size(), 0);
    map
}

fn get_db_file_name(db_name: &str, vbid: Vbid, rev: u64) -> String {
    format!("{}/{}.couch.{}", db_name, vbid, rev)
}

const LOCAL_DOC_KEY_VBSTATE: &str = "_local/vbstate";

fn get_local_vb_state(db: &mut Db) -> serde_json::Value {
    let doc: couchstore::LocalDoc = db.open_local_document(LOCAL_DOC_KEY_VBSTATE).unwrap();
    let json = doc.json.unwrap();
    serde_json::from_slice(&json).unwrap()
}

#[cfg(test)]
mod test {
    use super::*;

    /// Test that a store can be initialised from an existing travel sample bucket
    #[test]
    fn test_new() {
        let config = CouchKVStoreConfig {
            max_vbuckets: 1024,
            db_name: "../test-data/travel-sample".to_string(),
            max_shards: 1,
            shard_id: 0,
        };
        CouchKVStore::new(config);
    }
}

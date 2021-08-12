use rocksdb::{Options, DB, Error, perf};
use crate::utils::{get_data_dir, xor_two_values};
use discv5::enr::NodeId;
use rusqlite::{Connection, Result};
use std::fs;
use log::{error};

pub struct PortalStorageConfig {

    pub storage_capacity_kb: u64,
    pub node_id: NodeId,

}

pub struct PortalStorage {

    node_id: NodeId,
    storage_capacity_kb: u64,
    data_radius: u64,
    farthest_key: Option<String>,
    db: rocksdb::DB,
    meta_db: rusqlite::Connection,
    capacity_reached: bool

}

impl PortalStorage {

    pub fn new(config: &PortalStorageConfig) -> Result<Self, String> {

        // Create DB interfaces
        let db = PortalStorage::setup_rocksdb();
        let meta_db = PortalStorage::setup_sqlite();

        // Initialize the instance
        let storage = Self {
            node_id: config.node_id,
            storage_capacity_kb: config.storage_capacity_kb,
            data_radius: u64::MAX,
            db: db,
            farthest_key: None,
            meta_db: meta_db,
            capacity_reached: false
        };

        Ok(storage)

    }

    fn setup_rocksdb() -> DB {

        let data_path_root: String = get_data_dir().to_owned();
        let data_suffix: &str = "/rocksdb";
        let data_path = data_path_root + data_suffix;

        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        DB::open(&db_opts, data_path).unwrap()

    }

    fn setup_sqlite() -> rusqlite::Connection {

        let data_path_root: String = get_data_dir().to_owned();
        let data_suffix: &str = "/trin.sqlite";
        let data_path = data_path_root + data_suffix;

        let conn = Connection::open(data_path).unwrap();

        conn.execute(
            "create table if not exists content_keys (
                id integer primary key,
                content_key integer
            )",
            [],
        ).unwrap();

        conn

    }
    
    pub fn should_store(&self, key: &String) -> bool {

        if self.data_radius < u64::MAX {
            self.distance_to_key(key) < self.data_radius
        } else {
            true
        }

    }

    // 1.) Don't store data outside the radius.
    // 2.) Store the data, and then if we're at capacity, drop the farthest and find the new farthest.
    // 3.) Initialize or update farthest_key if necessary.
    // 4.) Check whether we've gone over capacity.
    pub fn store(&mut self, key: &String, value: &String) {

        if !self.should_store(key) {
            return;
        }

        self.db.put(key, value).expect("Failed to write to DB");

        // Take the first 8 bytes, turn them into a u64, insert them.
        let key_as_u64: u64 = PortalStorage::byte_vector_to_u64(key.clone().into_bytes());
        self.meta_db.execute(
            "INSERT INTO content_keys (content_key) values (?1)",
            [key_as_u64],
        ).unwrap();

        if self.capacity_reached {

            let key_to_remove = &self.farthest_key;
            self.db.delete(key_to_remove.as_ref().unwrap()).expect("Failed to delete key.");
            let key_to_remove_as_u64 = PortalStorage::byte_vector_to_u64(key_to_remove.clone().unwrap().into_bytes());
            self.meta_db.execute(
                "DELETE FROM content_keys
                 WHERE content_key = (?1)",
                [key_to_remove_as_u64],
            ).unwrap();
            
            match self.find_farthest() {
                Err(e) => {
                    error!("Failed to find farthest: {}", e);
                },
                Ok(farthest) => {
                    self.farthest_key = Some(farthest.clone());
                    self.data_radius = self.distance_to_key(&farthest);
                }
            }

        } else {

            let data_usage = self.get_total_storage_usage_kb();
            if data_usage > self.storage_capacity_kb {
              self.capacity_reached = true;
            }

        }

        match self.farthest_key.as_ref() {
            None => {
                self.farthest_key = Some(key.to_string());
            },
            Some(farthest) => {
                if self.distance_to_key(key) > self.distance_to_key(&farthest) {
                    self.farthest_key = Some(key.clone());
                }
            }
        }

    }

    pub fn get(&self, key: &String) -> Result<Option<Vec<u8>>, Error> {

        self.db.get(key)

    }

    pub fn get_current_radius(&self) -> u64 {

        self.data_radius

    }

    pub fn get_total_storage_usage_kb(&self) -> u64 {

        let p: perf::MemoryUsageStats = perf::get_memory_usage_stats(Some(&[&self.db]), None)
                                            .expect("Failed to get memory usage statistics.");

        let db_total = self.get_total_size_of_directory_in_bytes(get_data_dir()).unwrap();

        let total_in_bytes = p.mem_table_total + db_total;

        ( total_in_bytes / 1000 ) as u64

    }

    pub fn find_farthest(&self) -> Result<String, String> {

        let node_id_u64 = PortalStorage::byte_vector_to_u64(self.node_id.raw().to_vec());

        //TODO: Write working SQL to query from content_key column, order by XOR with node_id value, take 1.
        let mut query = self.meta_db.prepare(
            "FROM content_keys (content_key) values (?1)",
        ).unwrap();

        let results = query.query_map([node_id_u64], |row| {
            Ok(ContentKey {
                key: row.get(0)?
            })
        });

        let content_key = results.unwrap().next().unwrap().unwrap().key;

        Ok(content_key)

    }

    fn get_total_size_of_directory_in_bytes(&self, path: String) -> std::io::Result<u64> {

        let metadata = fs::metadata(&path).unwrap();
        let mut size = metadata.len();

        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                let dir = entry?;
                let path_string = dir.path().into_os_string().into_string();
                size += self.get_total_size_of_directory_in_bytes(path_string.unwrap())?;
            }
        }

        Ok(size)

    }

    pub fn distance_to_key(&self, key: &String) -> u64 {

        let byte_vector = xor_two_values(
            key.as_bytes(), &self.node_id.raw().to_vec()
        );

        PortalStorage::byte_vector_to_u64(byte_vector)
        
    }

    // Takes the most significant 8 bytes of a vector and casts them into a u64.
    // Useful in this class when the full bytes represent a u256, and for most purposes we only
    // need to compare the most significant 8 bytes of the u256 to compare 
    // relative distances. The equivalent of a conversion from nanometers to meters.
    fn byte_vector_to_u64(vec: Vec<u8>) -> u64 {

        if vec.len() < 8 {
            println!("Error: XOR returned less than 8 bytes.");
            return 0;
        }

        let mut array: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0];
        for (index, byte) in vec.iter().take(8).enumerate() {
            array[index] = byte.clone();
        }
      
        u64::from_be_bytes(array)

    }

}

struct ContentKey {
    key: String
}

#[cfg(test)]
mod test {

    use super::*;

    #[test]
    fn test_new() {

        let storage_config = PortalStorageConfig {
            storage_capacity_kb: 100,
            node_id: NodeId::random(),
        };
        let _ = PortalStorage::new(&storage_config);

    }

    #[test]
    fn test_store() {

        let storage_config = PortalStorageConfig {
            storage_capacity_kb: 100,
            node_id: NodeId::random(),
        };
        let mut storage = PortalStorage::new(&storage_config).unwrap();

        let key: String = "YlHPPvteGytjbPHbrMOVlK3Z90IcO4UR".to_string();
        let value: String = "OGFWs179fWnqmjvHQFGHszXloc3Wzdb4".to_string();
        storage.store(&key, &value);

    }

    #[test]
    fn test_get_data() {

        let storage_config = PortalStorageConfig {
            storage_capacity_kb: 100,
            node_id: NodeId::random(),
        };
        let mut storage = PortalStorage::new(&storage_config).unwrap();
        let key: String = "YlHPPvteGytjbPHbrMOVlK3Z90IcO4UR".to_string();
        let value: String = "OGFWs179fWnqmjvHQFGHszXloc3Wzdb4".to_string();
        storage.store(&key, &value);


        let result = storage.get(&key);

        println!("{}", String::from_utf8(result.unwrap().unwrap()).unwrap());

    }

    #[test]
    fn test_get_total_storage() {

        let storage_config = PortalStorageConfig {
            storage_capacity_kb: 100,
            node_id: NodeId::random(),
        };
        let mut storage = PortalStorage::new(&storage_config).unwrap();

        let key: String = "YlHPPvteGytjbPHbrMOVlK3Z90IcO4UR".to_string();
        let value: String = "OGFWs179fWnqmjvHQFGHszXloc3Wzdb4".to_string();
        storage.store(&key, &value);

        let kb = storage.get_total_storage_usage_kb();

        println!("{}", kb);

    }

    #[test]
    fn test_distance_to_key() {
        
        let storage_config = PortalStorageConfig {
            storage_capacity_kb: 100,
            node_id: NodeId::random(),
        };
        let storage = PortalStorage::new(&storage_config).unwrap();

        let key: String = "YlHPPvteGytjbPHbrMOVlK3Z90IcO4UR".to_string();
        let distance = storage.distance_to_key(&key);

        println!("Distance to key: {}", distance);

    }

}


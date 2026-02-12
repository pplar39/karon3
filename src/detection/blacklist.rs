use anyhow::Result;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::RwLock;

#[derive(Debug, Serialize, Deserialize, Default)]
struct BlacklistData {
    deployers: Vec<String>,
    last_updated: Option<String>,
}

pub struct Blacklist {
    deployers: RwLock<HashSet<Pubkey>>,
    file_path: String,
}

impl Blacklist {
    pub fn load(path: &str) -> Result<Self> {
        let path_obj = Path::new(path);
        let (deployer_set, file_path) = if path_obj.exists() {
            let content = fs::read_to_string(path)?;
            let data: BlacklistData = serde_json::from_str(&content)?;
            let mut set = HashSet::new();
            for d in data.deployers {
                if let Ok(pubkey) = Pubkey::from_str(&d) {
                    set.insert(pubkey);
                }
            }
            (set, path.to_string())
        } else {
            (HashSet::new(), path.to_string())
        };

        Ok(Self {
            deployers: RwLock::new(deployer_set),
            file_path,
        })
    }

    pub fn is_blacklisted(&self, deployer: &Pubkey) -> bool {
        let set = self.deployers.read().unwrap();
        set.contains(deployer)
    }

    pub fn add(&self, deployer: Pubkey, _reason: &str) -> Result<()> {
        {
            let mut set = self.deployers.write().unwrap();
            set.insert(deployer);
        }
        self.save()
    }

    pub fn save(&self) -> Result<()> {
        let set = self.deployers.read().unwrap();
        let deployers: Vec<String> = set.iter().map(|p| p.to_string()).collect();
        let data = BlacklistData {
            deployers,
            last_updated: Some(chrono::Utc::now().to_rfc3339()),
        };
        let content = serde_json::to_string_pretty(&data)?;
        fs::write(&self.file_path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blacklist_basic() -> Result<()> {
        let test_file = "test_blacklist.json";

        let _ = fs::remove_file(test_file);

        let blacklist = Blacklist::load(test_file)?;
        let pubkey = Pubkey::new_unique();

        assert!(!blacklist.is_blacklisted(&pubkey));
        blacklist.add(pubkey, "test rug")?;
        assert!(blacklist.is_blacklisted(&pubkey));

        let blacklist2 = Blacklist::load(test_file)?;
        assert!(blacklist2.is_blacklisted(&pubkey));

        fs::remove_file(test_file)?;

        Ok(())
    }
}

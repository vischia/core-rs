use ::std::collections::HashMap;
use ::serde::{ser, de};
use ::error::{TResult, TError};
use ::crypto::Key;
use ::models::model::Model;
use ::models::protected::{Keyfinder, Protected};
use ::models::sync_record::SyncAction;
use ::sync::sync_model::{self, SyncModel, MemorySaver};
use ::turtl::Turtl;
use ::jedi::{self, Value};

/// An enum used to 
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyType {
    #[serde(rename = "s")]
    Space,
    #[serde(rename = "b")]
    Board,
    #[serde(rename = "u")]
    User,
}

impl KeyType {
    pub fn from_string(s: String) -> TResult<Self> {
        let val = Value::String(s);
        Ok(jedi::from_val(val)?)
    }
}

/// Used as an easy object to reference other keys
#[derive(Clone)]
pub struct KeyRef<T: Clone> {
    /// The object id this key is for
    pub id: String,
    /// The object type (s = space, u = user)
    pub ty: KeyType,
    /// encrypted key (Base64-encoded)
    pub k: T,
}

impl<T: Default + Clone> KeyRef<T> {
    /// Create a new keyref
    pub fn new(id: String, ty: KeyType, k: T) -> Self {
        KeyRef {
            id: id,
            ty: ty,
            k: k,
        }
    }
}

impl ser::Serialize for KeyRef<String> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: ser::Serializer
    {
        let mut hash: HashMap<String, String> = HashMap::with_capacity(2);
        let type_str = match jedi::to_val(&self.ty) {
            Ok(x) => {
                match x {
                    Value::String(x) => x,
                    _ => return Err(ser::Error::custom(format!("KeyRef.serialize() -- error stringifying `ty` field"))),
                }
            },
            Err(_) => return Err(ser::Error::custom(format!("KeyRef.serialize() -- error stringifying `ty` field"))),
        };
        hash.insert(type_str, self.id.clone());
        hash.insert(String::from("k"), self.k.clone());
        hash.serialize(serializer)
    }
}

impl<'de> de::Deserialize<'de> for KeyRef<String> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: de::Deserializer<'de>
    {
        de::Deserialize::deserialize(deserializer)
            .and_then(|mut x: HashMap<String, String>| {
                let key = match x.remove(&String::from("k")) {
                    Some(x) => x,
                    None => return Err(de::Error::invalid_value(de::Unexpected::Map, &"KeyRef.deserialize() -- missing `k` field")),
                };
                let mut keyref: KeyRef<String> = KeyRef::new(String::from(""), KeyType::User, key);
                let typekey = match x.keys().next() {
                    Some(k) => k.clone(),
                    None => return Err(de::Error::invalid_value(de::Unexpected::Map, &"KeyRef.deserialize() -- missing type field")),
                };
                let ty: KeyType = match KeyType::from_string(typekey.clone()) {
                    Ok(x) => x,
                    Err(_) => return Err(de::Error::invalid_value(de::Unexpected::Str(&typekey.as_str()), &"KeyRef.deserialize() -- bad field")),
                };
                let id = x.remove(&typekey).unwrap();
                keyref.id = id;
                keyref.ty = ty;
                Ok(keyref)
            })
    }
}

protected! {
    #[derive(Serialize, Deserialize)]
    #[protected_modeltype(keychain)]
    pub struct KeychainEntry {
        #[serde(rename = "type")]
        #[protected_field(public)]
        pub ty: String,
        #[protected_field(public)]
        pub item_id: String,
        #[serde(with = "::util::ser::int_converter")]
        #[protected_field(public)]
        pub user_id: String,

        #[serde(skip_serializing_if = "Option::is_none")]
        #[protected_field(private)]
        pub k: Option<Key>,
    }
}

make_storable!(KeychainEntry, "keychain");
impl SyncModel for KeychainEntry {}
impl Keyfinder for KeychainEntry {}
impl MemorySaver for KeychainEntry {
    fn mem_update(self, turtl: &Turtl, action: SyncAction) -> TResult<()> {
        match action {
            SyncAction::Add | SyncAction::Edit => {
                let key = match self.k.as_ref() {
                    Some(x) => x,
                    None => return TErr!(TError::MissingField(String::from("Keychain.k"))),
                };
                // upsert our key
                let mut profile_guard = lockw!(turtl.profile);
                profile_guard.keychain.upsert_key(turtl, &self.item_id, key, &self.ty)?;
            }
            SyncAction::Delete => {
                let mut profile_guard = lockw!(turtl.profile);
                profile_guard.keychain.remove_entry(&self.item_id, None)?;
            }
            _ => {}
        }
        Ok(())

    }
}

#[derive(Debug)]
pub struct Keychain {
    pub entries: Vec<KeychainEntry>,
}

impl Keychain {
    /// Create an empty Keychain
    pub fn new() -> Keychain {
        Keychain {
            entries: Vec::new(),
        }
    }

    /// Upsert a key to the keychain
    fn upsert_key_impl(&mut self, turtl: &Turtl, item_id: &String, key: &Key, ty: &String, save: bool, skip_remote_sync: bool) -> TResult<()> {
        let (user_id, user_key) = {
            let user_guard = lockr!(turtl.user);
            let id = user_guard.id_or_else()?;
            let key = user_guard.key_or_else()?;
            (id, key)
        };
        let remove = {
            let existing = self.find_entry(item_id);
            match existing {
                Some(entry) => {
                    if entry.k.is_some() && entry.k.as_ref().unwrap() == key {
                        return Ok(());
                    }
                    true
                },
                None => false,
            }
        };
        if save && remove {
            self.remove_entry(item_id, Some((turtl, skip_remote_sync)))?;
        }
        let mut entry = KeychainEntry::new();
        entry.set_key(Some(user_key.clone()));
        entry.ty = ty.clone();
        entry.user_id = user_id.clone();
        entry.item_id = item_id.clone();
        entry.k = Some(key.clone());
        if save {
            sync_model::save_model(SyncAction::Add, turtl, &mut entry, skip_remote_sync)?;
        } else {
            entry.generate_id()?;
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Upsert a key to the keychain, don't save
    pub fn upsert_key(&mut self, turtl: &Turtl, item_id: &String, key: &Key, ty: &String) -> TResult<()> {
        self.upsert_key_impl(turtl, item_id, key, ty, false, true)
    }

    /// Upsert a key to the keychain, then save (sync)
    pub fn upsert_key_save(&mut self, turtl: &Turtl, item_id: &String, key: &Key, ty: &String, skip_remote_sync: bool) -> TResult<()> {
        self.upsert_key_impl(turtl, item_id, key, ty, true, skip_remote_sync)
    }

    /// Remove a keychain entry
    pub fn remove_entry(&mut self, item_id: &String, sync_save: Option<(&Turtl, bool)>) -> TResult<()> {
        match sync_save {
            Some((turtl, skip_remote_sync)) => {
                for entry in &mut self.entries {
                    if &entry.item_id != item_id { continue; }
                    sync_model::delete_model::<KeychainEntry>(turtl, entry.id().unwrap(), skip_remote_sync)?;
                }
            },
            None => {},
        }
        self.entries.retain(|entry| {
            &entry.item_id != item_id
        });
        Ok(())
    }

    /// Find the KeychainEntry matching the given item id
    pub fn find_entry<'a>(&'a self, item_id: &String) -> Option<&'a KeychainEntry> {
        for entry in &self.entries {
            if &entry.item_id == item_id {
                return Some(entry);
            }
        }
        None
    }

    /// Find the key matching a given item id
    pub fn find_key(&self, item_id: &String) -> Option<Key> {
        match self.find_entry(item_id) {
            Some(entry) => {
                if !entry.k.is_some() { return None; }
                Some(entry.k.as_ref().unwrap().clone())
            },
            None => {
                None
            },
        }
    }

    /// Find ALL matching keys for an object.
    pub fn find_all_entries(&self, item_id: &String) -> Vec<Key> {
        let mut found = Vec::with_capacity(2);
        for entry in &self.entries {
            if !entry.k.is_some() { continue; }
            if &entry.item_id == item_id {
                found.push(entry.k.as_ref().unwrap().clone());
            }
        }
        found
    }
}

// NOTE: for the following two functions, instead of saving to
// Turtl.profile.keychain directly, we create a temp keychain and save the
// changes to it, which in turns invokes MemorySaver (adding/removing the key
// to the Turtl.profile.keychain object).
//
// if we don't do it this way, then we have a write lock on
// Turtl.profile when the MemorySaver runs for the new key and we get
// a deadlock on any model that uses add_to_keychain() == true.
//
// so this, might seem a bit roundabout, but it lets us use MemorySaver
// for adding keys to the in-mem keychain so that logic can live in just
// one place.
//
// also keep in mind, there are a few places where we circumvent deadlocks by
// triggering app events that run outside of the current thread, however we
// DON'T want to do that here because keys should *always* be managed internally
// by the core and shouldn't be sent out (even to the UI). since the UI has the
// ability to listen on any messaging channel, we avoid sending keydata using
// this method.
// >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>
/// Save a key to the keychain for the current logged in user
pub fn save_key(turtl: &Turtl, item_id: &String, key: &Key, ty: &String, skip_remote_sync: bool) -> TResult<()> {
    let mut tmp_keychain = Keychain::new();
    tmp_keychain.upsert_key_save(turtl, item_id, key, ty, skip_remote_sync)
}

/// Remove a key from the keychain for the current logged in user
pub fn remove_key(turtl: &Turtl, item_id: &String, skip_remote_sync: bool) -> TResult<()> {
    let mut tmp_keychain = Keychain::new();
    tmp_keychain.remove_entry(item_id, Some((turtl, skip_remote_sync)))
}
// <<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<


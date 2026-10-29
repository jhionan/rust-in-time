//! Optional Herdr session metadata, with no desktop application control.
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

#[derive(Default)]
pub struct Resolver {
    path: PathBuf,
    sessions: HashMap<String, String>,
}
impl Resolver {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            sessions: HashMap::new(),
        }
    }
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config/herdr/session.json")
    }
    fn refresh(&mut self) -> BTreeMap<String, String> {
        let parsed = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(value) = parsed else {
            self.sessions.clear();
            return BTreeMap::new();
        };
        let Some(workspaces) = value["workspaces"].as_array() else {
            self.sessions.clear();
            return BTreeMap::new();
        };
        let mut spaces = BTreeMap::new();
        for ws in workspaces {
            let Some(id) = ws["id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            spaces.insert(
                id.to_owned(),
                ws["custom_name"].as_str().unwrap_or("").to_owned(),
            );
            if let Some(tabs) = ws["tabs"].as_array() {
                for tab in tabs {
                    if let Some(panes) = tab["panes"].as_object() {
                        for pane in panes.values() {
                            if let Some(session) = pane["agent_session"]["value"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                            {
                                self.sessions.insert(session.to_owned(), id.to_owned());
                            }
                        }
                    }
                }
            }
        }
        spaces
    }
    pub fn space_for_session(&mut self, id: &str) -> String {
        let spaces = self.refresh();
        self.sessions
            .get(id)
            .filter(|s| spaces.contains_key(*s))
            .cloned()
            .unwrap_or_default()
    }
    pub fn space_for_pane(&mut self, id: &str) -> String {
        let spaces = self.refresh();
        let id = id.split(':').next().unwrap_or("");
        if spaces.contains_key(id) {
            id.to_owned()
        } else {
            String::new()
        }
    }
}
pub fn list() -> Result<Value> {
    let mut resolver = Resolver::new(Resolver::default_path());
    Ok(json!(
        resolver
            .refresh()
            .into_iter()
            .map(|(id, name)| json!({"id":id,"name":name}))
            .collect::<Vec<_>>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_rotated_sessions_but_fails_closed_on_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path,r#"{"workspaces":[{"id":"w1","tabs":[{"panes":{"p":{"agent_session":{"value":"old"}}}}]}]}"#).unwrap();
        let mut r = Resolver::new(path.clone());
        assert_eq!(r.space_for_session("old"), "w1");
        std::fs::write(&path, r#"{"workspaces":[{"id":"w1","tabs":[]}]}"#).unwrap();
        assert_eq!(r.space_for_session("old"), "w1");
        assert_eq!(r.space_for_pane("w1:p2"), "w1");
        std::fs::write(&path, "{").unwrap();
        assert_eq!(r.space_for_session("old"), "");
    }
}

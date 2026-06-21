use std::collections::HashMap;

/// Provides a bidirectional map between type names and type_ids.
/// type_ids start from 1 (0 is reserved for undefined). Assignments are persisted
/// alongside the schema so that adding or removing a type never shifts the id of an
/// existing type — otherwise already-stored nodes would be read as the wrong type.
#[derive(Debug, Clone, Default)]
pub struct SchemaRegistry {
    node_name_to_id: HashMap<String, u16>,
    node_id_to_name: HashMap<u16, String>,
    edge_name_to_id: HashMap<String, u16>,
    edge_id_to_name: HashMap<u16, String>,
}

impl SchemaRegistry {
    /// Builds a registry from persisted (name, type_id) assignments.
    pub fn from_assignments(
        node_assignments: &[(String, u16)],
        edge_assignments: &[(String, u16)],
    ) -> Self {
        let mut node_name_to_id = HashMap::new();
        let mut node_id_to_name = HashMap::new();
        let mut edge_name_to_id = HashMap::new();
        let mut edge_id_to_name = HashMap::new();

        for (name, id) in node_assignments {
            node_name_to_id.insert(name.clone(), *id);
            node_id_to_name.insert(*id, name.clone());
        }
        for (name, id) in edge_assignments {
            edge_name_to_id.insert(name.clone(), *id);
            edge_id_to_name.insert(*id, name.clone());
        }

        Self {
            node_name_to_id,
            node_id_to_name,
            edge_name_to_id,
            edge_id_to_name,
        }
    }

    pub fn node_type_id(&self, name: &str) -> Option<u16> {
        self.node_name_to_id.get(name).copied()
    }

    pub fn edge_type_id(&self, name: &str) -> Option<u16> {
        self.edge_name_to_id.get(name).copied()
    }

    pub fn node_type_name(&self, id: u16) -> Option<&str> {
        self.node_id_to_name.get(&id).map(|s| s.as_str())
    }

    pub fn edge_type_name(&self, id: u16) -> Option<&str> {
        self.edge_id_to_name.get(&id).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SchemaRegistry {
        SchemaRegistry::from_assignments(
            &[("User".to_string(), 1), ("Project".to_string(), 2)],
            &[("OWNS".to_string(), 1), ("AUTHORED".to_string(), 2)],
        )
    }

    #[test]
    fn node_name_to_id_roundtrip() {
        let r = registry();
        let id = r.node_type_id("User").unwrap();
        assert_eq!(r.node_type_name(id), Some("User"));
    }

    #[test]
    fn edge_name_to_id_roundtrip() {
        let r = registry();
        let id = r.edge_type_id("OWNS").unwrap();
        assert_eq!(r.edge_type_name(id), Some("OWNS"));
    }

    #[test]
    fn ids_are_unique() {
        let r = registry();
        let user_id = r.node_type_id("User").unwrap();
        let project_id = r.node_type_id("Project").unwrap();
        assert_ne!(user_id, project_id);
    }

    #[test]
    fn unknown_name_returns_none() {
        let r = registry();
        assert!(r.node_type_id("Ghost").is_none());
        assert!(r.edge_type_id("UNKNOWN").is_none());
    }
}

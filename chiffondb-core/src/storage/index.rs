/// Property lookup by topology scan.
///
/// There is no persistent or in-memory index: `find`/`find_all`/`rids_of_type` scan the
/// topology segment (via the bounded page cache) and read each candidate node's properties
/// on demand. This keeps resident memory bounded by the page-cache budget rather than
/// growing with node count × property count, at the cost of O(nodes) lookups.
use std::collections::HashMap;

use serde_json::Value;

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::label_index::LabelIndex;
use crate::storage::page::RecordId;
use crate::storage::topology::TopologyStore;
use crate::storage::value::PropertyStore;

/// Returns true if a node's stored property `key` equals `value`.
///
/// Property values are scalars (schema field types: String/Int/Float/Bool/…), for which
/// `Value`'s structural equality coincides with the JSON-string equality the former
/// in-memory index keyed on — so query semantics are unchanged while avoiding a
/// per-node string allocation.
fn props_match(props: &HashMap<String, Value>, key: &str, value: &Value) -> bool {
    props.get(key).is_some_and(|stored| stored == value)
}

/// Returns the node RecordIds registered under `type_id` via the tier-1 label index.
///
/// Key = all labels (design §11 tier 1): this returns nodes whose primary type *or* any
/// additional/dynamic label is `type_id`, in O(matches) — not a full topology scan.
pub fn rids_of_type(
    _topo: &TopologyStore,
    file: &mut DatabaseFile,
    type_id: u16,
) -> Result<Vec<RecordId>, GraphError> {
    LabelIndex::new(file).get(type_id)
}

/// Returns all live nodes of `type_id` whose `key` property equals `value`.
pub fn find_all(
    topo: &TopologyStore,
    file: &mut DatabaseFile,
    type_id: u16,
    key: &str,
    value: &Value,
) -> Result<Vec<RecordId>, GraphError> {
    let mut out = Vec::new();
    for rid in topo.live_node_rids(file)? {
        let node = topo.read_node(file, rid)?;
        if node.node_type_id != type_id {
            continue;
        }
        let props = match node.property_ref {
            None => continue,
            Some(pref) => PropertyStore::read(file, pref)?,
        };
        if props_match(&props, key, value) {
            out.push(rid);
        }
    }
    Ok(out)
}

/// Returns the first live node of `type_id` whose `key` property equals `value`.
pub fn find(
    topo: &TopologyStore,
    file: &mut DatabaseFile,
    type_id: u16,
    key: &str,
    value: &Value,
) -> Result<Option<RecordId>, GraphError> {
    for rid in topo.live_node_rids(file)? {
        let node = topo.read_node(file, rid)?;
        if node.node_type_id != type_id {
            continue;
        }
        let props = match node.property_ref {
            None => continue,
            Some(pref) => PropertyStore::read(file, pref)?,
        };
        if props_match(&props, key, value) {
            return Ok(Some(rid));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Builds an in-memory DB. Topology pages map on demand through directories.
    fn make() -> (TopologyStore, DatabaseFile) {
        let file = DatabaseFile::create_in_memory().unwrap();
        (TopologyStore::new(), file)
    }

    fn insert(
        topo: &mut TopologyStore,
        file: &mut DatabaseFile,
        type_id: u16,
        pairs: &[(&str, Value)],
    ) -> RecordId {
        let props: HashMap<String, Value> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let pref = PropertyStore::write(file, &props).unwrap();
        let rid = topo.alloc_node(file, type_id).unwrap();
        let mut node = topo.read_node(file, rid).unwrap();
        node.property_ref = Some(pref);
        topo.write_node(file, &node).unwrap();
        // rids_of_type now reads the label index, so mirror what Database::insert_node does.
        LabelIndex::new(file).add(type_id, rid).unwrap();
        rid
    }

    #[test]
    fn find_matches_by_type_key_value() {
        let (mut topo, mut f) = make();
        let r1 = insert(
            &mut topo,
            &mut f,
            1,
            &[("id", json!("u1")), ("name", json!("Alice"))],
        );

        assert_eq!(
            find(&topo, &mut f, 1, "id", &json!("u1")).unwrap(),
            Some(r1)
        );
        assert_eq!(
            find(&topo, &mut f, 1, "name", &json!("Alice")).unwrap(),
            Some(r1)
        );
        assert_eq!(find(&topo, &mut f, 1, "id", &json!("nope")).unwrap(), None);
        // Different type_id must not match.
        assert_eq!(find(&topo, &mut f, 2, "id", &json!("u1")).unwrap(), None);
    }

    #[test]
    fn find_all_returns_every_match() {
        let (mut topo, mut f) = make();
        let r1 = insert(&mut topo, &mut f, 1, &[("role", json!("admin"))]);
        let r2 = insert(&mut topo, &mut f, 1, &[("role", json!("admin"))]);
        let _other = insert(&mut topo, &mut f, 1, &[("role", json!("user"))]);

        let all = find_all(&topo, &mut f, 1, "role", &json!("admin")).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&r1));
        assert!(all.contains(&r2));
    }

    #[test]
    fn rids_of_type_filters_by_type() {
        let (mut topo, mut f) = make();
        let r1 = insert(&mut topo, &mut f, 1, &[("id", json!("a"))]);
        let r2 = insert(&mut topo, &mut f, 1, &[("id", json!("b"))]);
        let _e = insert(&mut topo, &mut f, 2, &[("id", json!("c"))]);

        let rids = rids_of_type(&topo, &mut f, 1).unwrap();
        assert_eq!(rids.len(), 2);
        assert!(rids.contains(&r1));
        assert!(rids.contains(&r2));
    }

    #[test]
    fn find_after_free_does_not_match() {
        // A freed node slot must not show up (live_node_rids excludes it).
        let (mut topo, mut f) = make();
        let r1 = insert(&mut topo, &mut f, 1, &[("id", json!("u1"))]);
        topo.free_node(&mut f, r1).unwrap();
        assert_eq!(find(&topo, &mut f, 1, "id", &json!("u1")).unwrap(), None);
    }
}

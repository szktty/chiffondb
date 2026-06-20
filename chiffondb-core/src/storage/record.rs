use crate::error::GraphError;
use crate::storage::page::{PageId, RecordId, SlotId};

/// Sentinel value representing an absent RecordId (all 7 bytes set to 0xFF).
const NULL_RECORD: [u8; 7] = [0xFF; 7];

/// Serializes a RecordId into 7 bytes (page_id: 4 bytes + slot_id: 2 bytes + padding: 1 byte).
pub fn encode_record_id(rid: Option<RecordId>) -> [u8; 7] {
    match rid {
        None => NULL_RECORD,
        Some(r) => {
            let mut buf = [0u8; 7];
            buf[0..4].copy_from_slice(&r.page_id.0.to_le_bytes());
            buf[4..6].copy_from_slice(&r.slot_id.0.to_le_bytes());
            buf[6] = 0;
            buf
        }
    }
}

pub fn decode_record_id(buf: &[u8; 7]) -> Option<RecordId> {
    if *buf == NULL_RECORD {
        return None;
    }
    let page_id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let slot_id = u16::from_le_bytes(buf[4..6].try_into().unwrap());
    Some(RecordId {
        page_id: PageId(page_id),
        slot_id: SlotId(slot_id),
    })
}

/// Serializes a RecordId into 6 bytes (page_id: 4 bytes + slot_id: 2 bytes).
pub fn encode_record_id_6(rid: RecordId) -> [u8; 6] {
    let mut buf = [0u8; 6];
    buf[0..4].copy_from_slice(&rid.page_id.0.to_le_bytes());
    buf[4..6].copy_from_slice(&rid.slot_id.0.to_le_bytes());
    buf
}

pub fn decode_record_id_6(buf: &[u8; 6]) -> RecordId {
    let page_id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let slot_id = u16::from_le_bytes(buf[4..6].try_into().unwrap());
    RecordId {
        page_id: PageId(page_id),
        slot_id: SlotId(slot_id),
    }
}

pub const NODE_RECORD_SIZE: usize = 64;

/// Node record (fixed size: 64 bytes).
///
/// Layout:
///  [0..6]   id (RecordId, 6 bytes)
///  [6..8]   node_type_id (u16)
///  [8..15]  first_out_edge (`Option<RecordId>`, 7 bytes)
///  [15..22] first_in_edge (`Option<RecordId>`, 7 bytes)
///  [22..29] property_ref (`Option<RecordId>`, 7 bytes)
///  [29..36] label_ref (`Option<RecordId>`, 7 bytes) — reference to the additional label list
///  \[36\]   flags (1 byte)
///  [37..64] reserved (27 bytes)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub id: RecordId,
    pub node_type_id: u16,
    pub first_out_edge: Option<RecordId>,
    pub first_in_edge: Option<RecordId>,
    pub property_ref: Option<RecordId>,
    pub label_ref: Option<RecordId>,
    pub flags: u8,
}

impl NodeRecord {
    pub fn serialize(&self) -> [u8; NODE_RECORD_SIZE] {
        let mut buf = [0u8; NODE_RECORD_SIZE];
        buf[0..6].copy_from_slice(&encode_record_id_6(self.id));
        buf[6..8].copy_from_slice(&self.node_type_id.to_le_bytes());
        buf[8..15].copy_from_slice(&encode_record_id(self.first_out_edge));
        buf[15..22].copy_from_slice(&encode_record_id(self.first_in_edge));
        buf[22..29].copy_from_slice(&encode_record_id(self.property_ref));
        buf[29..36].copy_from_slice(&encode_record_id(self.label_ref));
        buf[36] = self.flags;
        buf
    }

    pub fn deserialize(buf: &[u8; NODE_RECORD_SIZE]) -> Result<Self, GraphError> {
        let id = decode_record_id_6(buf[0..6].try_into().unwrap());
        let node_type_id = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        let first_out_edge = decode_record_id(buf[8..15].try_into().unwrap());
        let first_in_edge = decode_record_id(buf[15..22].try_into().unwrap());
        let property_ref = decode_record_id(buf[22..29].try_into().unwrap());
        let label_ref = decode_record_id(buf[29..36].try_into().unwrap());
        let flags = buf[36];
        Ok(NodeRecord {
            id,
            node_type_id,
            first_out_edge,
            first_in_edge,
            property_ref,
            label_ref,
            flags,
        })
    }
}

pub const EDGE_RECORD_SIZE: usize = 64;

/// Edge record (fixed size: 64 bytes).
///
/// Layout:
///  [0..6]   id (RecordId, 6 bytes)
///  [6..8]   edge_type_id (u16)
///  [8..14]  from_node (RecordId, 6 bytes)
///  [14..20] to_node (RecordId, 6 bytes)
///  [20..27] next_out_edge (`Option<RecordId>`, 7 bytes)
///  [27..34] next_in_edge (`Option<RecordId>`, 7 bytes)
///  [34..41] property_ref (`Option<RecordId>`, 7 bytes)
///  \[41\]   flags (1 byte)
///  [42..49] label_ref (`Option<RecordId>`, 7 bytes) — reference to the additional label list
///  [49..64] reserved (15 bytes)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeRecord {
    pub id: RecordId,
    pub edge_type_id: u16,
    pub from_node: RecordId,
    pub to_node: RecordId,
    pub next_out_edge: Option<RecordId>,
    pub next_in_edge: Option<RecordId>,
    pub property_ref: Option<RecordId>,
    pub flags: u8,
    pub label_ref: Option<RecordId>,
}

impl EdgeRecord {
    pub fn serialize(&self) -> [u8; EDGE_RECORD_SIZE] {
        let mut buf = [0u8; EDGE_RECORD_SIZE];
        buf[0..6].copy_from_slice(&encode_record_id_6(self.id));
        buf[6..8].copy_from_slice(&self.edge_type_id.to_le_bytes());
        buf[8..14].copy_from_slice(&encode_record_id_6(self.from_node));
        buf[14..20].copy_from_slice(&encode_record_id_6(self.to_node));
        buf[20..27].copy_from_slice(&encode_record_id(self.next_out_edge));
        buf[27..34].copy_from_slice(&encode_record_id(self.next_in_edge));
        buf[34..41].copy_from_slice(&encode_record_id(self.property_ref));
        buf[41] = self.flags;
        buf[42..49].copy_from_slice(&encode_record_id(self.label_ref));
        buf
    }

    pub fn deserialize(buf: &[u8; EDGE_RECORD_SIZE]) -> Result<Self, GraphError> {
        let id = decode_record_id_6(buf[0..6].try_into().unwrap());
        let edge_type_id = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        let from_node = decode_record_id_6(buf[8..14].try_into().unwrap());
        let to_node = decode_record_id_6(buf[14..20].try_into().unwrap());
        let next_out_edge = decode_record_id(buf[20..27].try_into().unwrap());
        let next_in_edge = decode_record_id(buf[27..34].try_into().unwrap());
        let property_ref = decode_record_id(buf[34..41].try_into().unwrap());
        let flags = buf[41];
        let label_ref = decode_record_id(buf[42..49].try_into().unwrap());
        Ok(EdgeRecord {
            id,
            edge_type_id,
            from_node,
            to_node,
            next_out_edge,
            next_in_edge,
            property_ref,
            flags,
            label_ref,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(p: u32, s: u16) -> RecordId {
        RecordId::new(p, s)
    }

    #[test]
    fn node_record_roundtrip() {
        let node = NodeRecord {
            id: rid(1, 0),
            node_type_id: 42,
            first_out_edge: Some(rid(2, 3)),
            first_in_edge: None,
            property_ref: Some(rid(5, 7)),
            label_ref: None,
            flags: 0,
        };
        let buf = node.serialize();
        assert_eq!(buf.len(), NODE_RECORD_SIZE);
        let decoded = NodeRecord::deserialize(&buf).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn node_record_with_label_ref_roundtrip() {
        let node = NodeRecord {
            id: rid(1, 0),
            node_type_id: 10,
            first_out_edge: None,
            first_in_edge: None,
            property_ref: Some(rid(3, 1)),
            label_ref: Some(rid(4, 2)),
            flags: 0,
        };
        let buf = node.serialize();
        assert_eq!(buf.len(), NODE_RECORD_SIZE);
        let decoded = NodeRecord::deserialize(&buf).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn edge_record_roundtrip() {
        let edge = EdgeRecord {
            id: rid(1, 0),
            edge_type_id: 7,
            from_node: rid(10, 2),
            to_node: rid(20, 5),
            next_out_edge: Some(rid(1, 1)),
            next_in_edge: None,
            property_ref: None,
            flags: 0,
            label_ref: None,
        };
        let buf = edge.serialize();
        assert_eq!(buf.len(), EDGE_RECORD_SIZE);
        let decoded = EdgeRecord::deserialize(&buf).unwrap();
        assert_eq!(edge, decoded);
    }

    #[test]
    fn edge_record_with_label_ref_roundtrip() {
        let edge = EdgeRecord {
            id: rid(1, 0),
            edge_type_id: 7,
            from_node: rid(10, 2),
            to_node: rid(20, 5),
            next_out_edge: None,
            next_in_edge: None,
            property_ref: None,
            flags: 0,
            label_ref: Some(rid(5, 3)),
        };
        let buf = edge.serialize();
        assert_eq!(buf.len(), EDGE_RECORD_SIZE);
        let decoded = EdgeRecord::deserialize(&buf).unwrap();
        assert_eq!(edge, decoded);
    }

    #[test]
    fn null_record_id_encodes_as_sentinel() {
        let encoded = encode_record_id(None);
        assert_eq!(encoded, NULL_RECORD);
        assert_eq!(decode_record_id(&encoded), None);
    }
}

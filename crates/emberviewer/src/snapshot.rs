//! Snapshots of a provider tree.
//!
//! A snapshot holds the values of the parameters below the branches that the
//! operator selects. Each parameter keeps the full chain of elements from the
//! device root down to itself. Because of this chain, a restore finds each
//! parameter again on its own: the operator does not select a root. The operator
//! can still restore a part of a snapshot, because each parameter is applied
//! separately.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use ember_proto::glow::{self, Real, Value};
use ember_web_proto::WireValue;
use serde::{Deserialize, Serialize};

use crate::model::{Entry, Kind, TreeModel};
use crate::wire::{value_from_wire, value_to_wire};

/// Version of the snapshot file format. Increase it if the layout changes.
pub const FORMAT: u32 = 1;

/// The suggested file extension for a snapshot.
pub const EXTENSION: &str = "json";

/// A saved set of parameter values, with the path to each of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Format version. See [`FORMAT`].
    pub format: u32,
    /// Name of the provider at capture time. For information only.
    #[serde(default)]
    pub provider: String,
    /// Address of the provider at capture time. For information only.
    #[serde(default)]
    pub address: String,
    /// Capture time, UTC.
    #[serde(default)]
    pub created: String,
    /// Element chains that start at the device root.
    #[serde(default)]
    pub roots: Vec<SnapNode>,
}

/// One element in a snapshot: a node on the way down, or a parameter with a
/// value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapNode {
    /// The Ember+ number of the element.
    pub number: u32,
    /// The identifier of the element. A restore uses it first, because a device
    /// can change the numbers.
    #[serde(default)]
    pub identifier: String,
    /// The captured value. Parameters only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<WireValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SnapNode>,
}

/// One captured parameter, as a flat record.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The numeric path from the device root, as captured.
    pub numbers: Vec<u32>,
    /// The identifier of each element on that path.
    pub ids: Vec<String>,
    /// The captured value.
    pub value: Value,
}

impl Item {
    /// The path as text, for the operator. Example: `Root/Gains/Fader 1`.
    pub fn label(&self) -> String {
        let parts: Vec<String> = self
            .ids
            .iter()
            .zip(&self.numbers)
            .map(|(id, n)| {
                if id.is_empty() {
                    n.to_string()
                } else {
                    id.clone()
                }
            })
            .collect();
        parts.join("/")
    }
}

/// Where a captured element is on the live device.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// The element is at this path.
    Path(Vec<u32>),
    /// The tree does not hold this part yet. Ask this node for its children.
    Fetch(Vec<u32>),
    /// The device does not have the element.
    Missing,
}

/// What a restore can do with one captured parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Write this value to this path.
    Write {
        path: Vec<u32>,
        value: Value,
        current: Option<Value>,
    },
    /// The device already holds the captured value.
    Same { path: Vec<u32> },
    /// The device does not let you write this parameter.
    ReadOnly { path: Vec<u32> },
    /// The captured value does not fit the parameter on the device.
    BadType { path: Vec<u32> },
    /// The tree does not hold this part yet. Ask this node for its children.
    Loading { fetch: Vec<u32> },
    /// The device does not have the parameter.
    Missing,
}

impl Plan {
    /// The live path, if the parameter was found.
    pub fn path(&self) -> Option<&[u32]> {
        match self {
            Plan::Write { path, .. }
            | Plan::Same { path }
            | Plan::ReadOnly { path }
            | Plan::BadType { path } => Some(path),
            _ => None,
        }
    }

    /// Whether a restore can write this parameter.
    pub fn is_writable(&self) -> bool {
        matches!(self, Plan::Write { .. })
    }

    /// A short status word for the operator.
    pub fn status(&self) -> &'static str {
        match self {
            Plan::Write { .. } => "changes",
            Plan::Same { .. } => "same",
            Plan::ReadOnly { .. } => "read-only",
            Plan::BadType { .. } => "type",
            Plan::Loading { .. } => "loading…",
            Plan::Missing => "not found",
        }
    }
}

impl Snapshot {
    /// Capture every parameter below the given branches.
    ///
    /// A branch that is a parameter captures that parameter alone. The result
    /// keeps the elements above each parameter, up to the device root.
    pub fn capture(
        tree: &TreeModel,
        branches: &[Vec<u32>],
        provider: &str,
        address: &str,
        with_read_only: bool,
    ) -> Snapshot {
        let mut leaves: BTreeMap<Vec<u32>, WireValue> = BTreeMap::new();
        for branch in branches {
            collect(tree, branch, with_read_only, &mut leaves);
        }
        let mut roots: Vec<SnapNode> = Vec::new();
        for (path, value) in leaves {
            insert_chain(tree, &mut roots, &path, value);
        }
        Snapshot {
            format: FORMAT,
            provider: provider.to_string(),
            address: address.to_string(),
            created: format_unix_utc(now_unix()),
            roots,
        }
    }

    /// The captured parameters as a flat list, in path order.
    pub fn items(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for root in &self.roots {
            flatten(root, &mut Vec::new(), &mut Vec::new(), &mut out);
        }
        out
    }

    /// How many parameters the snapshot holds.
    pub fn len(&self) -> usize {
        self.roots.iter().map(count_values).sum()
    }

    /// Read a snapshot from a file.
    pub fn load_from(path: &Path) -> Result<Snapshot, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let snap: Snapshot = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if snap.format > FORMAT {
            return Err(format!(
                "the file uses format {} - this version reads up to {FORMAT}",
                snap.format
            ));
        }
        Ok(snap)
    }

    /// Write the snapshot to a file. The write is atomic.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::fsutil::write_atomic(path, &json).map_err(|e| e.to_string())
    }
}

/// How many parameters a capture of these branches keeps. The branches must not
/// overlap, which is what the selection in the save window makes sure of.
pub fn count_capturable(tree: &TreeModel, branches: &[Vec<u32>], with_read_only: bool) -> usize {
    branches
        .iter()
        .map(|branch| count_below(tree, branch, with_read_only))
        .sum()
}

/// How many parameters below `path` a capture keeps.
fn count_below(tree: &TreeModel, path: &[u32], with_read_only: bool) -> usize {
    let Some(entry) = tree.get(path) else {
        return 0;
    };
    if entry.kind == Kind::Parameter {
        return usize::from(entry.value.is_some() && capturable(entry, with_read_only));
    }
    entry
        .children
        .iter()
        .map(|child| count_below(tree, child, with_read_only))
        .sum()
}

/// Add every parameter below `path` to `out`.
fn collect(
    tree: &TreeModel,
    path: &[u32],
    with_read_only: bool,
    out: &mut BTreeMap<Vec<u32>, WireValue>,
) {
    let Some(entry) = tree.get(path) else {
        return;
    };
    if entry.kind == Kind::Parameter {
        if let Some(value) = &entry.value {
            if capturable(entry, with_read_only) {
                out.insert(path.to_vec(), value_to_wire(value));
            }
        }
        return;
    }
    for child in &entry.children {
        collect(tree, child, with_read_only, out);
    }
}

/// Whether a snapshot keeps this parameter.
///
/// A trigger has no state, and a stream carries live measurements. Neither is
/// of use in a snapshot. Read-only parameters are kept only on request, because
/// a restore cannot write them.
fn capturable(entry: &Entry, with_read_only: bool) -> bool {
    entry.stream_identifier.is_none()
        && entry.param_type != Some(glow::parameter_type::TRIGGER)
        && (with_read_only || entry.is_writable())
}

/// Add one parameter and the elements above it to the snapshot tree.
fn insert_chain(tree: &TreeModel, roots: &mut Vec<SnapNode>, path: &[u32], value: WireValue) {
    let mut level = roots;
    for depth in 0..path.len() {
        let number = path[depth];
        let index = match level.iter().position(|n| n.number == number) {
            Some(i) => i,
            None => {
                level.push(SnapNode {
                    number,
                    identifier: tree
                        .get(&path[..=depth])
                        .map(|e| e.identifier.clone())
                        .unwrap_or_default(),
                    value: None,
                    children: Vec::new(),
                });
                level.len() - 1
            }
        };
        let node = &mut level[index];
        if depth + 1 == path.len() {
            node.value = Some(value);
            return;
        }
        level = &mut node.children;
    }
}

/// Walk a snapshot node and add each parameter to `out`.
fn flatten(node: &SnapNode, numbers: &mut Vec<u32>, ids: &mut Vec<String>, out: &mut Vec<Item>) {
    numbers.push(node.number);
    ids.push(node.identifier.clone());
    if let Some(wire) = &node.value {
        if let Some(value) = value_from_wire(wire) {
            out.push(Item {
                numbers: numbers.clone(),
                ids: ids.clone(),
                value,
            });
        }
    }
    for child in &node.children {
        flatten(child, numbers, ids, out);
    }
    numbers.pop();
    ids.pop();
}

/// How many values a snapshot node holds, itself and below.
fn count_values(node: &SnapNode) -> usize {
    usize::from(node.value.is_some()) + node.children.iter().map(count_values).sum::<usize>()
}

/// Finds the live path of a captured element.
///
/// The search goes down one level at a time. At each level it looks for the
/// identifier first, then for the number. A device that renumbers its elements,
/// or that inserts new ones, thus stays supported. Results are kept, because a
/// snapshot usually holds many parameters that share their upper levels.
pub struct Resolver<'a> {
    tree: &'a TreeModel,
    cache: HashMap<Vec<u32>, Found>,
}

impl<'a> Resolver<'a> {
    pub fn new(tree: &'a TreeModel) -> Self {
        Resolver {
            tree,
            cache: HashMap::new(),
        }
    }

    /// Find one captured parameter on the live device.
    pub fn find(&mut self, item: &Item) -> Found {
        let mut live: Vec<u32> = Vec::new();
        for depth in 0..item.numbers.len() {
            let key = item.numbers[..=depth].to_vec();
            let step = match self.cache.get(&key) {
                Some(found) => found.clone(),
                None => {
                    let found = step(self.tree, &live, item.numbers[depth], &item.ids[depth]);
                    self.cache.insert(key, found.clone());
                    found
                }
            };
            match step {
                Found::Path(path) => live = path,
                other => return other,
            }
        }
        Found::Path(live)
    }
}

/// Find one child of `parent` by identifier, then by number.
fn step(tree: &TreeModel, parent: &[u32], number: u32, id: &str) -> Found {
    let children: &[Vec<u32>] = if parent.is_empty() {
        &tree.roots
    } else {
        match tree.get(parent) {
            Some(entry) => &entry.children,
            // The parent is gone from the tree: ask for it again.
            None => return Found::Fetch(parent.to_vec()),
        }
    };
    if !id.is_empty() {
        if let Some(path) = children
            .iter()
            .find(|c| tree.get(c).is_some_and(|e| e.identifier == id))
        {
            return Found::Path(path.clone());
        }
    }
    if let Some(path) = children.iter().find(|c| c.last() == Some(&number)) {
        return Found::Path(path.clone());
    }
    if children.is_empty() {
        // The app fetches a directory in one step, so an empty list means that
        // this part of the tree is not read yet.
        Found::Fetch(parent.to_vec())
    } else {
        Found::Missing
    }
}

/// Match every captured parameter against the live tree.
pub fn plan_all(tree: &TreeModel, items: &[Item]) -> Vec<Plan> {
    let mut resolver = Resolver::new(tree);
    items
        .iter()
        .map(|item| match resolver.find(item) {
            Found::Fetch(path) => Plan::Loading { fetch: path },
            Found::Missing => Plan::Missing,
            Found::Path(path) => match tree.get(&path) {
                None => Plan::Missing,
                Some(entry) if entry.kind != Kind::Parameter => Plan::Missing,
                Some(entry) => plan_for(entry, &path, &item.value),
            },
        })
        .collect()
}

/// Decide what to do with one captured value on a live parameter.
fn plan_for(entry: &Entry, path: &[u32], value: &Value) -> Plan {
    let path = path.to_vec();
    let Some(value) = coerce(entry, value) else {
        return Plan::BadType { path };
    };
    if entry
        .value
        .as_ref()
        .is_some_and(|current| same_value(current, &value))
    {
        return Plan::Same { path };
    }
    if !entry.is_writable() {
        return Plan::ReadOnly { path };
    }
    Plan::Write {
        current: entry.value.clone(),
        path,
        value,
    }
}

/// Change a captured value into the type that the live parameter uses.
///
/// A file keeps a real as a decimal number, and a device can also report a type
/// that differs from the one at capture time. `None` if the value does not fit.
pub fn coerce(entry: &Entry, value: &Value) -> Option<Value> {
    use glow::parameter_type as pt;
    let target = entry
        .param_type
        .or_else(|| entry.value.as_ref().map(type_of));
    let Some(target) = target else {
        return Some(value.clone()); // Type unknown: send the value as it is.
    };
    Some(match (target, value) {
        (pt::INTEGER | pt::ENUM, Value::Integer(i)) => Value::Integer(*i),
        (pt::INTEGER | pt::ENUM, Value::Real(r)) => Value::Integer(r.to_f64().round() as i64),
        (pt::INTEGER | pt::ENUM, Value::Boolean(b)) => Value::Integer(i64::from(*b)),
        (pt::REAL, Value::Real(r)) => Value::Real(r.clone()),
        (pt::REAL, Value::Integer(i)) => Value::Real(Real::from_f64(*i as f64)),
        (pt::STRING, Value::String(s)) => Value::String(s.clone()),
        (pt::BOOLEAN, Value::Boolean(b)) => Value::Boolean(*b),
        (pt::BOOLEAN, Value::Integer(i)) => Value::Boolean(*i != 0),
        (pt::OCTETS, Value::Octets(o)) => Value::Octets(o.clone()),
        _ => return None,
    })
}

/// The Ember+ type of a value.
fn type_of(value: &Value) -> i32 {
    use glow::parameter_type as pt;
    match value {
        Value::Integer(_) => pt::INTEGER,
        Value::Real(_) => pt::REAL,
        Value::String(_) => pt::STRING,
        Value::Boolean(_) => pt::BOOLEAN,
        Value::Octets(_) => pt::OCTETS,
    }
}

/// Compare two values. Two reals are compared as numbers, because the same
/// number can have more than one set of content octets.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Real(x), Value::Real(y)) => x.to_f64() == y.to_f64(),
        _ => a == b,
    }
}

/// Seconds since the Unix epoch.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A Unix time as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn format_unix_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let time = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        time / 3600,
        (time / 60) % 60,
        time % 60
    )
}

/// The civil date of a day count that starts at 1970-01-01.
/// From Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year
    let mp = (5 * doy + 2) / 153; // month, shifted so that March is 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe as i64 + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_proto::glow::{
        access, parameter_type, NodeContents, ParameterContents, QualifiedNode, QualifiedParameter,
        RelativeOid, Root, RootElement, RootElementCollection, RootElementEntry,
    };

    fn node_doc(path: &[u32], id: &str) -> Root {
        Root::from_element(RootElement::QualifiedNode(QualifiedNode {
            path: RelativeOid::from_arcs(path),
            contents: Some(NodeContents {
                identifier: Some(id.into()),
                ..Default::default()
            }),
            children: None,
        }))
    }

    fn param_doc(path: &[u32], id: &str, value: Value, access: i32, ptype: i32) -> Root {
        Root::Elements(RootElementCollection(vec![RootElementEntry(
            RootElement::QualifiedParameter(QualifiedParameter {
                path: RelativeOid::from_arcs(path),
                contents: Some(ParameterContents {
                    identifier: Some(id.into()),
                    value_: Some(value),
                    access: Some(access),
                    r#type: Some(ptype),
                    ..Default::default()
                }),
                children: None,
            }),
        )]))
    }

    /// A small device tree: `Root/Audio/{gain,mute,meter}` and `Root/Info/name`.
    fn sample_tree() -> TreeModel {
        let mut tree = TreeModel::new();
        tree.merge(node_doc(&[1], "Root"));
        tree.merge(node_doc(&[1, 1], "Audio"));
        tree.merge(node_doc(&[1, 2], "Info"));
        tree.merge(param_doc(
            &[1, 1, 1],
            "gain",
            Value::Integer(-6),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));
        tree.merge(param_doc(
            &[1, 1, 2],
            "mute",
            Value::Boolean(false),
            access::READ_WRITE,
            parameter_type::BOOLEAN,
        ));
        tree.merge(param_doc(
            &[1, 1, 3],
            "meter",
            Value::Real(Real::from_f64(-20.0)),
            access::READ,
            parameter_type::REAL,
        ));
        tree.merge(param_doc(
            &[1, 2, 1],
            "name",
            Value::String("Desk A".into()),
            access::READ_WRITE,
            parameter_type::STRING,
        ));
        tree
    }

    /// A capture of one branch keeps the elements up to the root, and skips the
    /// read-only parameters.
    #[test]
    fn capture_keeps_the_chain_to_the_root() {
        let tree = sample_tree();
        let snap = Snapshot::capture(&tree, &[vec![1, 1]], "Desk", "10.0.0.1:9000", false);
        assert_eq!(snap.len(), 2, "read-only meter must not be captured");
        let items = snap.items();
        assert_eq!(items[0].numbers, vec![1, 1, 1]);
        assert_eq!(items[0].ids, vec!["Root", "Audio", "gain"]);
        assert_eq!(items[0].label(), "Root/Audio/gain");
        assert_eq!(items[1].numbers, vec![1, 1, 2]);
        // The branch above the selection is in the file.
        assert_eq!(snap.roots.len(), 1);
        assert_eq!(snap.roots[0].identifier, "Root");
        assert_eq!(snap.roots[0].children[0].identifier, "Audio");
    }

    /// Read-only parameters are captured on request, but a restore leaves them.
    #[test]
    fn capture_can_keep_read_only_parameters() {
        let tree = sample_tree();
        let snap = Snapshot::capture(&tree, &[vec![1, 1]], "Desk", "", true);
        assert_eq!(snap.len(), 3);
        let plans = plan_all(&tree, &snap.items());
        assert!(matches!(plans[2], Plan::Same { .. }));
    }

    /// The count that the save window shows agrees with the file.
    #[test]
    fn count_agrees_with_the_capture() {
        let tree = sample_tree();
        let branches = [vec![1, 1], vec![1, 2]];
        for read_only in [false, true] {
            assert_eq!(
                count_capturable(&tree, &branches, read_only),
                Snapshot::capture(&tree, &branches, "", "", read_only).len()
            );
        }
    }

    /// Two branches share their upper levels in one file.
    #[test]
    fn capture_merges_branches_that_share_a_parent() {
        let tree = sample_tree();
        let snap = Snapshot::capture(&tree, &[vec![1, 1], vec![1, 2]], "Desk", "", false);
        assert_eq!(snap.roots.len(), 1);
        assert_eq!(snap.roots[0].children.len(), 2);
        assert_eq!(snap.len(), 3);
    }

    /// A snapshot survives a write and a read.
    #[test]
    fn snapshot_round_trips_through_json() {
        let tree = sample_tree();
        let snap = Snapshot::capture(&tree, &[vec![1]], "Desk", "10.0.0.1:9000", false);
        let json = serde_json::to_vec(&snap).unwrap();
        let back: Snapshot = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.provider, "Desk");
        assert_eq!(back.items(), snap.items());
    }

    /// A restore finds each parameter without help, and reports the values that
    /// change.
    #[test]
    fn restore_plans_only_the_values_that_differ() {
        let tree = sample_tree();
        let snap = Snapshot::capture(&tree, &[vec![1]], "Desk", "", false);
        // The device moves on after the capture.
        let mut tree = tree;
        tree.merge(param_doc(
            &[1, 1, 1],
            "gain",
            Value::Integer(0),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));
        let items = snap.items();
        let plans = plan_all(&tree, &items);
        assert_eq!(
            plans[0],
            Plan::Write {
                path: vec![1, 1, 1],
                value: Value::Integer(-6),
                current: Some(Value::Integer(0)),
            }
        );
        assert!(matches!(plans[1], Plan::Same { .. }));
        assert!(matches!(plans[2], Plan::Same { .. }));
    }

    /// A full cycle: capture, write the plan back into a device that moved on,
    /// and check that nothing is left to write.
    #[test]
    fn restore_converges_on_the_captured_state() {
        let snap = Snapshot::capture(&sample_tree(), &[vec![1]], "Desk", "", false);
        // The device now holds other values, and a new parameter of its own.
        let mut device = sample_tree();
        device.merge(param_doc(
            &[1, 1, 1],
            "gain",
            Value::Integer(12),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));
        device.merge(param_doc(
            &[1, 1, 2],
            "mute",
            Value::Boolean(true),
            access::READ_WRITE,
            parameter_type::BOOLEAN,
        ));
        device.merge(param_doc(
            &[1, 2, 2],
            "extra",
            Value::Integer(5),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));

        let items = snap.items();
        let plans = plan_all(&device, &items);
        let writes: Vec<(Vec<u32>, Value)> = plans
            .iter()
            .filter_map(|plan| match plan {
                Plan::Write { path, value, .. } => Some((path.clone(), value.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 2, "gain and mute must change: {plans:?}");

        // The device answers each set with the new value.
        for (path, value) in writes {
            let entry = device.get(&path).unwrap();
            let (id, ptype) = (entry.identifier.clone(), entry.param_type.unwrap());
            device.merge(param_doc(&path, &id, value, access::READ_WRITE, ptype));
        }
        let plans = plan_all(&device, &items);
        assert!(
            plans.iter().all(|p| matches!(p, Plan::Same { .. })),
            "a second restore must have nothing to do: {plans:?}"
        );
        // The parameter that the snapshot does not hold keeps its value.
        assert_eq!(
            device.get(&[1, 2, 2]).and_then(|e| e.value.clone()),
            Some(Value::Integer(5))
        );
    }

    /// The device renumbers a node. The identifier still finds the parameter.
    #[test]
    fn restore_follows_a_renumbered_node() {
        let snap = Snapshot::capture(&sample_tree(), &[vec![1, 1]], "Desk", "", false);
        let mut tree = TreeModel::new();
        tree.merge(node_doc(&[1], "Root"));
        tree.merge(node_doc(&[1, 7], "Audio")); // was 1/1
        tree.merge(param_doc(
            &[1, 7, 1],
            "gain",
            Value::Integer(0),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));
        tree.merge(param_doc(
            &[1, 7, 2],
            "mute",
            Value::Boolean(false),
            access::READ_WRITE,
            parameter_type::BOOLEAN,
        ));
        let plans = plan_all(&tree, &snap.items());
        assert_eq!(plans[0].path(), Some(&[1, 7, 1][..]));
        assert!(plans[0].is_writable());
    }

    /// A parameter that the app did not read yet reports the node to fetch, and
    /// one that the device does not have reports "missing".
    #[test]
    fn restore_reports_loading_and_missing() {
        let snap = Snapshot::capture(&sample_tree(), &[vec![1, 1]], "Desk", "", false);
        // Only the root is known: the app must ask node [1] for its children.
        let mut tree = TreeModel::new();
        tree.merge(node_doc(&[1], "Root"));
        let plans = plan_all(&tree, &snap.items());
        assert_eq!(plans[0], Plan::Loading { fetch: vec![1] });

        // The device has the node, but not the parameters.
        tree.merge(node_doc(&[1, 1], "Audio"));
        tree.merge(param_doc(
            &[1, 1, 9],
            "other",
            Value::Integer(0),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));
        let plans = plan_all(&tree, &snap.items());
        assert_eq!(plans[0], Plan::Missing);
    }

    /// The file keeps a real as a decimal number. A restore fits it back to the
    /// type of the live parameter, and refuses a value that does not fit.
    #[test]
    fn restore_fits_the_value_to_the_live_type() {
        let mut tree = TreeModel::new();
        tree.merge(param_doc(
            &[1],
            "level",
            Value::Real(Real::from_f64(1.0)),
            access::READ_WRITE,
            parameter_type::REAL,
        ));
        let entry = tree.get(&[1]).unwrap();
        assert_eq!(
            coerce(entry, &Value::Integer(3)),
            Some(Value::Real(Real::from_f64(3.0)))
        );
        assert_eq!(coerce(entry, &Value::String("x".into())), None);

        // An integer parameter takes a real from the file.
        tree.merge(param_doc(
            &[2],
            "count",
            Value::Integer(1),
            access::READ_WRITE,
            parameter_type::INTEGER,
        ));
        let entry = tree.get(&[2]).unwrap();
        assert_eq!(
            coerce(entry, &Value::Real(Real::from_f64(2.6))),
            Some(Value::Integer(3))
        );
    }

    /// A newer file format is refused, not read in part.
    #[test]
    fn load_refuses_a_newer_format() {
        let dir = std::env::temp_dir().join(format!("emberviewer-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snap.json");
        std::fs::write(&path, br#"{"format":99,"roots":[]}"#).unwrap();
        assert!(Snapshot::load_from(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn utc_time_is_formatted() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_unix_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }
}

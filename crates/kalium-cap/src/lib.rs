//! Capability objects and per-instance capability tables.
//!
//! A capability names a kernel object ([`ObjectId`]), says what kind of
//! object it is ([`ObjectKind`]) and which operations the holder may perform
//! on it ([`Rights`]). Each instance owns a [`CapTable`]; Wasm code refers to
//! capabilities by their slot index ([`CapIndex`]), which is the `i32` the
//! host ABI passes around.
//!
//! The object registry (what an `ObjectId` refers to) lives in the kernel,
//! not here. This crate only enforces the rules on handles:
//!
//! - a slot that was never filled, or was dropped or taken, is not a live
//!   capability and every lookup of it fails with [`CapError::BadCap`];
//! - an operation that needs a kind or a right the capability lacks fails
//!   with [`CapError::Perm`];
//! - the table never holds more than its limit; [`CapError::NoMem`] when full;
//! - a derived capability never has more rights than its source and cannot
//!   carry `DERIVE` unless the source did;
//! - every capability has a derivation identity, its node in the object's
//!   [`Lineage`]; revoking a capability invalidates everything derived from
//!   it (transitively, wherever it went), nothing else, and takes effect on
//!   the next check ([`Lineage::is_valid`]); a chain is at most
//!   [`MAX_DEPTH`] derivations long.
//!
//! All inputs are treated as untrusted: nothing here panics on a bad index,
//! bad kind, bad rights value or unknown node.
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use kalium_abi::{errno, kind, limits, rights};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from capability table operations, each mapping to one ABI errno.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapError {
    /// The index is not a live slot (`EBADCAP`).
    BadCap,
    /// The slot exists but the capability is the wrong kind or lacks a
    /// required right (`EPERM`).
    Perm,
    /// The table is at its limit (`ENOMEM`).
    NoMem,
    /// A value that is not a valid kind or rights set (`EINVAL`).
    Invalid,
}

impl CapError {
    /// The ABI errno this error is reported as.
    pub const fn errno(self) -> i32 {
        match self {
            CapError::BadCap => errno::EBADCAP,
            CapError::Perm => errno::EPERM,
            CapError::NoMem => errno::ENOMEM,
            CapError::Invalid => errno::EINVAL,
        }
    }
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CapError::BadCap => "index is not a live capability slot",
            CapError::Perm => "capability is the wrong kind or lacks the right",
            CapError::NoMem => "capability table is full",
            CapError::Invalid => "invalid kind or rights value",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// Rights
// ---------------------------------------------------------------------------

/// A set of rights on an object: a newtype over the `u64` bit flags defined
/// in [`kalium_abi::rights`].
///
/// Invariant: only bits in [`kalium_abi::rights::MASK`] are ever set. Values
/// from untrusted input go through [`Rights::from_bits`] (rejects unknown
/// bits) or [`Rights::from_bits_truncate`] (drops them).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Rights(u64);

impl Rights {
    /// No rights at all.
    pub const NONE: Rights = Rights(0);
    /// `READ`.
    pub const READ: Rights = Rights(rights::READ);
    /// `WRITE`.
    pub const WRITE: Rights = Rights(rights::WRITE);
    /// `SEND`.
    pub const SEND: Rights = Rights(rights::SEND);
    /// `RECV`.
    pub const RECV: Rights = Rights(rights::RECV);
    /// `DERIVE`.
    pub const DERIVE: Rights = Rights(rights::DERIVE);
    /// `TRANSFER`.
    pub const TRANSFER: Rights = Rights(rights::TRANSFER);
    /// `KILL`.
    pub const KILL: Rights = Rights(rights::KILL);
    /// `WAIT`.
    pub const WAIT: Rights = Rights(rights::WAIT);
    /// `SPAWN`.
    pub const SPAWN: Rights = Rights(rights::SPAWN);
    /// `SNAPSHOT`.
    pub const SNAPSHOT: Rights = Rights(rights::SNAPSHOT);
    /// Every defined right.
    pub const ALL: Rights = Rights(rights::MASK);

    /// Builds a rights set, rejecting any bit the ABI does not define.
    pub const fn from_bits(bits: u64) -> Option<Rights> {
        if bits & !rights::MASK == 0 {
            Some(Rights(bits))
        } else {
            None
        }
    }

    /// Builds a rights set, silently dropping bits the ABI does not define.
    pub const fn from_bits_truncate(bits: u64) -> Rights {
        Rights(bits & rights::MASK)
    }

    /// The raw bits, as written by `cap_info` and read by `cap_derive`.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` if every right in `other` is also in `self`.
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// `true` if any right in `other` is in `self`.
    pub const fn intersects(self, other: Rights) -> bool {
        self.0 & other.0 != 0
    }

    /// The rights in both `self` and `other`.
    pub const fn intersection(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    /// The rights in either `self` or `other`.
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    /// The rights in `self` that are not in `other`.
    pub const fn difference(self, other: Rights) -> Rights {
        Rights(self.0 & !other.0)
    }

    /// `true` if no right is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl From<Rights> for u64 {
    fn from(r: Rights) -> u64 {
        r.0
    }
}

impl TryFrom<u64> for Rights {
    type Error = CapError;

    /// Rejects unknown bits with [`CapError::Invalid`].
    fn try_from(bits: u64) -> Result<Rights, CapError> {
        Rights::from_bits(bits).ok_or(CapError::Invalid)
    }
}

impl core::ops::BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Rights) {
        self.0 |= rhs.0;
    }
}

impl core::ops::BitAnd for Rights {
    type Output = Rights;
    fn bitand(self, rhs: Rights) -> Rights {
        self.intersection(rhs)
    }
}

impl core::ops::BitAndAssign for Rights {
    fn bitand_assign(&mut self, rhs: Rights) {
        self.0 &= rhs.0;
    }
}

impl core::ops::Sub for Rights {
    type Output = Rights;
    fn sub(self, rhs: Rights) -> Rights {
        self.difference(rhs)
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rights(")?;
        let mut first = true;
        for (bit, name) in rights::ALL {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("NONE")?;
        }
        f.write_str(")")
    }
}

// ---------------------------------------------------------------------------
// Object kinds and ids
// ---------------------------------------------------------------------------

/// The kind of object a capability refers to. The discriminants are the ABI
/// values from [`kalium_abi::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum ObjectKind {
    /// The UART console.
    Console = kind::CONSOLE,
    /// An IPC channel.
    Channel = kind::CHANNEL,
    /// A memory object.
    MemObj = kind::MEMOBJ,
    /// An instance.
    Instance = kind::INSTANCE,
    /// The authority to spawn instances.
    Spawn = kind::SPAWN,
    /// The captured state of an instance.
    Snapshot = kind::SNAPSHOT,
}

impl ObjectKind {
    /// Every kind, in ABI order.
    pub const ALL: [ObjectKind; 6] = [
        ObjectKind::Console,
        ObjectKind::Channel,
        ObjectKind::MemObj,
        ObjectKind::Instance,
        ObjectKind::Spawn,
        ObjectKind::Snapshot,
    ];

    /// The ABI value, as written by `cap_info`.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Parses an ABI value; `None` for anything the ABI does not define.
    pub const fn from_u32(v: u32) -> Option<ObjectKind> {
        match v {
            kind::CONSOLE => Some(ObjectKind::Console),
            kind::CHANNEL => Some(ObjectKind::Channel),
            kind::MEMOBJ => Some(ObjectKind::MemObj),
            kind::INSTANCE => Some(ObjectKind::Instance),
            kind::SPAWN => Some(ObjectKind::Spawn),
            kind::SNAPSHOT => Some(ObjectKind::Snapshot),
            _ => None,
        }
    }
}

impl From<ObjectKind> for u32 {
    fn from(k: ObjectKind) -> u32 {
        k.as_u32()
    }
}

impl TryFrom<u32> for ObjectKind {
    type Error = CapError;

    /// Rejects undefined values with [`CapError::Invalid`].
    fn try_from(v: u32) -> Result<ObjectKind, CapError> {
        ObjectKind::from_u32(v).ok_or(CapError::Invalid)
    }
}

/// A kernel-wide unique object identifier. The kernel's registry mints these
/// and never reuses one, so a capability can never silently come to refer to
/// a different object than the one it was created for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u64);

impl ObjectId {
    /// The raw id.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

/// An unforgeable handle: an object, its kind, the rights the holder has on
/// it, and its derivation identity. Capabilities are plain values; the only
/// way for an instance to get one is through the kernel (boot,
/// `channel_create`, `memobj_create`, `spawn`) or by transfer and derivation
/// from one it already holds.
///
/// The pair `(object, node)` is the capability's *lineage*: `node` names
/// its position in the object's derivation tree ([`Lineage`]). A derived
/// capability sits below the one it came from; a transfer moves the value
/// without changing it. Revoking a capability invalidates everything below
/// its node, wherever those capabilities are (see [`Lineage::revoke`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Capability {
    /// The object this capability refers to.
    pub object: ObjectId,
    /// What kind of object it is.
    pub kind: ObjectKind,
    /// What the holder may do with it.
    pub rights: Rights,
    /// Its node in the object's [`Lineage`].
    pub node: NodeId,
}

impl Capability {
    /// Builds a capability at `node`, a node of `object`'s lineage. Only the
    /// kernel (or tests) should call this; instances obtain capabilities
    /// through host calls.
    pub const fn new(
        object: ObjectId,
        kind: ObjectKind,
        rights: Rights,
        node: NodeId,
    ) -> Capability {
        Capability {
            object,
            kind,
            rights,
            node,
        }
    }

    /// The same object, kind and rights at another node of the lineage.
    pub const fn with_node(self, node: NodeId) -> Capability {
        Capability { node, ..self }
    }

    /// `true` if this capability has every right in `rights`.
    pub const fn has(&self, rights: Rights) -> bool {
        self.rights.contains(rights)
    }

    /// Checks kind and rights; [`CapError::Perm`] if either does not match.
    pub const fn check(&self, kind: ObjectKind, rights: Rights) -> Result<(), CapError> {
        if self.kind as u32 == kind as u32 && self.rights.contains(rights) {
            Ok(())
        } else {
            Err(CapError::Perm)
        }
    }

    /// The rights a derivation of `self` would carry, per `cap_derive`:
    ///
    /// > Needs `DERIVE`. New cap has `rights & old.rights` and never gains
    /// > `DERIVE` unless the old had it.
    ///
    /// Fails with [`CapError::Perm`] if `self` lacks `DERIVE`. Because the
    /// result is the intersection with `self.rights`, it can never have a
    /// right `self` lacks, `DERIVE` included; the holder may keep `DERIVE` in
    /// the derived capability only by asking for it. Checking this before
    /// allocating the child node keeps a refused derivation from touching
    /// the lineage.
    pub const fn derived_rights(&self, requested: Rights) -> Result<Rights, CapError> {
        if !self.rights.contains(Rights::DERIVE) {
            return Err(CapError::Perm);
        }
        Ok(self.rights.intersection(requested))
    }

    /// Mints a weaker capability on the same object at `child`, a node the
    /// object's lineage derived under `self.node` ([`Lineage::derive`]).
    /// The rights rule is [`derived_rights`](Capability::derived_rights).
    pub const fn derive(&self, requested: Rights, child: NodeId) -> Result<Capability, CapError> {
        match self.derived_rights(requested) {
            Ok(rights) => Ok(Capability {
                object: self.object,
                kind: self.kind,
                rights,
                node: child,
            }),
            Err(e) => Err(e),
        }
    }

    /// The `{kind, rights}` pair `cap_info` writes to linear memory.
    pub const fn info(&self) -> kalium_abi::layout::CapInfo {
        kalium_abi::layout::CapInfo {
            kind: self.kind.as_u32(),
            rights: self.rights.bits(),
        }
    }
}

// ---------------------------------------------------------------------------
// Lineage: derivation trees and revocation
// ---------------------------------------------------------------------------

/// A node of a [`Lineage`]: the derivation identity of a capability on one
/// object. Only meaningful together with the object (`Capability::object`)
/// whose lineage it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// The most derivation steps between a root and a leaf:
/// [`kalium_abi::limits::MAX_DERIVE_DEPTH`]. A root is at depth 0; deriving
/// from a node at this depth fails with [`CapError::NoMem`]. It bounds the
/// walk [`Lineage::is_valid`] makes on every capability check.
pub const MAX_DEPTH: u32 = limits::MAX_DERIVE_DEPTH;

#[derive(Clone, Copy, Debug)]
struct Node {
    parent: Option<NodeId>,
    /// The parent's `generation` when this node was derived. Once the
    /// parent has been revoked the two differ, and this node and everything
    /// below it are invalid.
    parent_generation: u32,
    /// Bumped by [`Lineage::revoke`]; cuts off every child derived before.
    generation: u32,
    depth: u32,
    /// Capabilities that carry this node (anywhere: tables, messages).
    refs: u32,
    /// Live child nodes. A node is freed once `refs` and `children` are
    /// both zero.
    children: u32,
}

/// The derivation tree of one object: who derived what from what, and what
/// has been revoked. The kernel keeps one per registered object.
///
/// Every capability minted fresh by the kernel (boot capabilities, the two
/// ends of `channel_create`, `memobj_create`, `spawn`) is a *root*;
/// `cap_derive` makes a *child* of the source's node; a transfer keeps the
/// node. [`revoke`](Lineage::revoke) on a node invalidates every node
/// below it, transitively, and nothing else: not the node itself, not its
/// siblings, not its ancestors. The holder of a capability can therefore
/// only cut off what was derived from it, and a transferred capability is
/// revoked together with the chain it came from.
///
/// Revocation is recorded as a generation on the revoked node; a child
/// remembers the generation it was derived under, and
/// [`is_valid`](Lineage::is_valid) walks a node's chain upwards (at most
/// [`MAX_DEPTH`] steps) looking for a mismatch. No table is scanned when a
/// capability is checked, and a node revoked once may derive again: the new
/// children are valid, the old ones stay dead.
///
/// Nodes are reference counted: [`retain`](Lineage::retain) and
/// [`release`](Lineage::release) follow the capabilities that carry a node,
/// a parent stays while it has children, and a node nothing refers to any
/// more is freed and its slot reused, so a derive/drop loop does not grow
/// the tree. A node freshly returned by [`root`](Lineage::root) or
/// [`derive`](Lineage::derive) has no reference yet: the caller retains it
/// when the capability is in place, or releases it if that failed.
///
/// Nothing here panics on an unknown node: lookups fail with
/// [`CapError::BadCap`], a release of an unknown node does nothing.
#[derive(Clone, Debug, Default)]
pub struct Lineage {
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    live: usize,
}

impl Lineage {
    /// Heap one live node takes in its tree: what a capability in flight
    /// (queued in a message, so in no table) keeps allocated beyond the
    /// `Capability` itself. The kernel charges it per queued capability
    /// (`kalium_ipc::Message::cost_of`); capabilities in tables are bounded
    /// by the tables' size.
    pub const NODE_BYTES: usize = core::mem::size_of::<Option<Node>>();

    /// An empty tree.
    pub const fn new() -> Lineage {
        Lineage {
            nodes: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    /// Number of live nodes.
    pub fn len(&self) -> usize {
        self.live
    }

    /// `true` if no node is live.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// `true` if `node` is a live node of this tree.
    pub fn contains(&self, node: NodeId) -> bool {
        self.get(node).is_some()
    }

    /// Derivation steps from the root, or `None` for an unknown node.
    pub fn depth(&self, node: NodeId) -> Option<u32> {
        self.get(node).map(|n| n.depth)
    }

    /// The node a node was derived from (`None` for a root or an unknown
    /// node).
    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.get(node).and_then(|n| n.parent)
    }

    /// Capabilities currently retaining `node`, or `None` if unknown.
    pub fn refs(&self, node: NodeId) -> Option<u32> {
        self.get(node).map(|n| n.refs)
    }

    fn get(&self, node: NodeId) -> Option<&Node> {
        self.nodes.get(node.0 as usize).and_then(Option::as_ref)
    }

    fn get_mut(&mut self, node: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(node.0 as usize).and_then(Option::as_mut)
    }

    fn alloc(&mut self, node: Node) -> Result<NodeId, CapError> {
        let id = match self.free.pop() {
            Some(i) => match self.nodes.get_mut(i as usize) {
                Some(slot @ None) => {
                    *slot = Some(node);
                    i
                }
                // Unreachable by the invariants; refuse rather than clobber.
                _ => return Err(CapError::NoMem),
            },
            None => {
                let i = u32::try_from(self.nodes.len()).map_err(|_| CapError::NoMem)?;
                // Fallible growth: a lineage grows on a host call
                // (`cap_derive`), which must report `ENOMEM` rather than
                // abort the kernel when the heap is short. The free list
                // is kept able to hold every node so that a later
                // release, which cannot fail, never has to grow it.
                let free_room = (self.nodes.len() + 1).saturating_sub(self.free.len());
                if self.nodes.try_reserve(1).is_err() || self.free.try_reserve(free_room).is_err() {
                    return Err(CapError::NoMem);
                }
                self.nodes.push(Some(node));
                i
            }
        };
        self.live += 1;
        Ok(NodeId(id))
    }

    /// A new root: the node of a capability the kernel mints fresh. It has
    /// no reference yet (see the type docs). [`CapError::NoMem`] if the
    /// index space is exhausted.
    pub fn root(&mut self) -> Result<NodeId, CapError> {
        self.alloc(Node {
            parent: None,
            parent_generation: 0,
            generation: 0,
            depth: 0,
            refs: 0,
            children: 0,
        })
    }

    /// A new child of `parent`: the node of a capability derived from one
    /// at `parent`. It has no reference yet (see the type docs).
    ///
    /// [`CapError::BadCap`] if `parent` is unknown, [`CapError::NoMem`] if
    /// `parent` is already [`MAX_DEPTH`] steps from its root. Deriving from
    /// a revoked node is allowed (the child is simply invalid, like its
    /// parent); the kernel refuses that earlier with `EBADCAP`.
    pub fn derive(&mut self, parent: NodeId) -> Result<NodeId, CapError> {
        let p = self.get(parent).ok_or(CapError::BadCap)?;
        if p.depth >= MAX_DEPTH {
            return Err(CapError::NoMem);
        }
        let child = Node {
            parent: Some(parent),
            parent_generation: p.generation,
            generation: 0,
            depth: p.depth + 1,
            refs: 0,
            children: 0,
        };
        let id = self.alloc(child)?;
        if let Some(p) = self.get_mut(parent) {
            p.children = p.children.saturating_add(1);
        }
        Ok(id)
    }

    /// One more capability carries `node`. [`CapError::BadCap`] if unknown.
    pub fn retain(&mut self, node: NodeId) -> Result<(), CapError> {
        let n = self.get_mut(node).ok_or(CapError::BadCap)?;
        n.refs = n.refs.checked_add(1).ok_or(CapError::NoMem)?;
        Ok(())
    }

    /// One capability fewer carries `node`. A node with no capability and no
    /// child left is freed, and so, in turn, is every ancestor that this
    /// was the last child of and nothing refers to. Unknown nodes are
    /// ignored.
    pub fn release(&mut self, node: NodeId) {
        let mut cur = node;
        let mut first = true;
        // Bounded by the depth of the chain.
        for _ in 0..=MAX_DEPTH {
            let Some(n) = self.get_mut(cur) else {
                return;
            };
            if first {
                n.refs = n.refs.saturating_sub(1);
                first = false;
            } else {
                n.children = n.children.saturating_sub(1);
            }
            if n.refs != 0 || n.children != 0 {
                return;
            }
            let parent = n.parent;
            if let Some(slot) = self.nodes.get_mut(cur.0 as usize) {
                *slot = None;
            }
            self.free.push(cur.0);
            self.live -= 1;
            match parent {
                Some(p) => cur = p,
                None => return,
            }
        }
    }

    /// Invalidates every node derived from `node`, transitively, and only
    /// those: `node` itself, its siblings and its ancestors are untouched,
    /// and children derived from `node` after this call are valid.
    /// [`CapError::BadCap`] if `node` is unknown; [`CapError::NoMem`] if the
    /// node has been revoked so often that its generation would wrap.
    pub fn revoke(&mut self, node: NodeId) -> Result<(), CapError> {
        let n = self.get_mut(node).ok_or(CapError::BadCap)?;
        n.generation = n.generation.checked_add(1).ok_or(CapError::NoMem)?;
        Ok(())
    }

    /// Whether [`revoke`](Lineage::revoke) of `node` would succeed, with
    /// the error it would fail with and nothing changed. The kernel asks
    /// before it settles what the revocation kills, so that a refused
    /// revocation (`NoMem` once the generation would wrap) invalidates
    /// nothing and touches no count.
    pub fn can_revoke(&self, node: NodeId) -> Result<(), CapError> {
        let n = self.get(node).ok_or(CapError::BadCap)?;
        n.generation
            .checked_add(1)
            .map(|_| ())
            .ok_or(CapError::NoMem)
    }

    /// [`revoke`](Lineage::revoke), reporting which of `held` (capabilities
    /// on this object, wherever they live: tables or messages in flight)
    /// stopped being valid by it. Those are exactly the capabilities whose
    /// effect on the object (a channel's endpoint counts) ends now rather
    /// than when they are eventually dropped. Capabilities that were already
    /// invalid, or on other objects, are not reported.
    pub fn revoke_among(
        &mut self,
        node: NodeId,
        held: &[Capability],
    ) -> Result<Vec<Capability>, CapError> {
        let was_valid: Vec<bool> = held.iter().map(|c| self.is_valid(c.node)).collect();
        self.revoke(node)?;
        Ok(held
            .iter()
            .zip(was_valid)
            .filter(|(c, was)| *was && !self.is_valid(c.node))
            .map(|(c, _)| *c)
            .collect())
    }

    /// `true` if `node` is live and no node above it has been revoked since
    /// the step below it was derived: the capability carrying it may be
    /// used. Walks the chain upwards, at most [`MAX_DEPTH`] steps. An
    /// unknown node is not valid.
    pub fn is_valid(&self, node: NodeId) -> bool {
        let mut cur = match self.get(node) {
            Some(n) => n,
            None => return false,
        };
        for _ in 0..=MAX_DEPTH {
            let Some(parent) = cur.parent else {
                return true;
            };
            let Some(p) = self.get(parent) else {
                return false;
            };
            if p.generation != cur.parent_generation {
                return false;
            }
            cur = p;
        }
        // A chain longer than the depth limit cannot exist.
        false
    }

    /// `true` if `node` is `ancestor` or below it (both must be live).
    pub fn is_descendant(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut cur = node;
        for _ in 0..=MAX_DEPTH {
            let Some(n) = self.get(cur) else {
                return false;
            };
            if cur == ancestor {
                return true;
            }
            match n.parent {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Checks the structural invariants; `true` means consistent.
    pub fn check_invariants(&self) -> bool {
        let live = self.nodes.iter().filter(|n| n.is_some()).count();
        if live != self.live || self.free.len() + live != self.nodes.len() {
            return false;
        }
        for (n, &f) in self.free.iter().enumerate() {
            if !matches!(self.nodes.get(f as usize), Some(None)) || self.free[..n].contains(&f) {
                return false;
            }
        }
        for (i, node) in self.nodes.iter().enumerate() {
            let Some(node) = node else { continue };
            if node.depth > MAX_DEPTH {
                return false;
            }
            let children = self
                .nodes
                .iter()
                .flatten()
                .filter(|c| c.parent == Some(NodeId(i as u32)))
                .count();
            if children != node.children as usize {
                return false;
            }
            match node.parent {
                Some(p) => match self.get(p) {
                    Some(parent) if parent.depth + 1 == node.depth => {}
                    _ => return false,
                },
                None if node.depth != 0 => return false,
                None => {}
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// CapIndex
// ---------------------------------------------------------------------------

/// A slot index into a [`CapTable`].
///
/// Invariant: the value is at most `i32::MAX`, so every `CapIndex` converts
/// losslessly to the ABI's `i32`, and every negative `i32` (including
/// [`kalium_abi::CAP_NONE`]) is rejected on the way in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapIndex(u32);

impl CapIndex {
    /// The largest representable index.
    pub const MAX: CapIndex = CapIndex(i32::MAX as u32);

    /// Builds an index; `None` if `raw` does not fit in an `i32`.
    pub const fn new(raw: u32) -> Option<CapIndex> {
        if raw <= Self::MAX.0 {
            Some(CapIndex(raw))
        } else {
            None
        }
    }

    /// The slot number.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The ABI representation.
    pub const fn as_i32(self) -> i32 {
        // Lossless by the invariant.
        self.0 as i32
    }

    /// The slot number as a `usize` for indexing.
    const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl TryFrom<i32> for CapIndex {
    type Error = CapError;

    /// Rejects negatives with [`CapError::BadCap`], since the ABI says
    /// `-1` is never valid and no slot can have a negative number.
    fn try_from(v: i32) -> Result<CapIndex, CapError> {
        u32::try_from(v).map(CapIndex).map_err(|_| CapError::BadCap)
    }
}

impl TryFrom<u32> for CapIndex {
    type Error = CapError;

    /// Rejects values above `i32::MAX` with [`CapError::BadCap`].
    fn try_from(v: u32) -> Result<CapIndex, CapError> {
        CapIndex::new(v).ok_or(CapError::BadCap)
    }
}

impl TryFrom<usize> for CapIndex {
    type Error = CapError;

    /// Rejects values above `i32::MAX` with [`CapError::BadCap`].
    fn try_from(v: usize) -> Result<CapIndex, CapError> {
        u32::try_from(v)
            .ok()
            .and_then(CapIndex::new)
            .ok_or(CapError::BadCap)
    }
}

impl From<CapIndex> for i32 {
    fn from(i: CapIndex) -> i32 {
        i.as_i32()
    }
}

impl From<CapIndex> for u32 {
    fn from(i: CapIndex) -> u32 {
        i.0
    }
}

impl From<CapIndex> for usize {
    fn from(i: CapIndex) -> usize {
        i.as_usize()
    }
}

// ---------------------------------------------------------------------------
// CapTable
// ---------------------------------------------------------------------------

/// The capability table of one instance.
///
/// Slots are numbered from 0. A freed slot (after [`CapTable::take`] or
/// [`CapTable::drop_slot`]) goes on a free list and is reused by the next
/// insert, lowest number first, so tables stay dense and indices small.
///
/// # Invariants
///
/// 1. `slots[i]` is `Some` exactly when slot `i` is live. A freed slot is set
///    to `None` immediately, so no API can read a capability out of a slot
///    that was dropped or taken: `get`, `check`, `take`, `derive` and
///    `drop_slot` all fail with [`CapError::BadCap`] on it until a later
///    `insert` fills it again.
/// 2. `free` holds each `None` slot below `slots.len()` exactly once and
///    nothing else; `live + free.len() == slots.len()`.
/// 3. `live <= limit`, and `limit <= CapIndex::MAX + 1`, so every index
///    handed out is a valid `CapIndex`.
///
/// # Slot reuse
///
/// Because slots are reused, an index that Wasm code kept after dropping or
/// transferring the capability may later name a *different* capability that
/// a subsequent insert put in that slot. That is a bug in the instance's own
/// code (like using a closed file descriptor) and it can only ever reach a
/// capability the same instance legitimately holds, so confinement is not
/// affected. The ABI's `i32` index leaves no room for a generation tag; a
/// stale index is never mistaken for a dropped capability because the
/// dropped capability no longer exists anywhere in the table.
#[derive(Clone, Debug)]
pub struct CapTable {
    slots: Vec<Option<Capability>>,
    free: Vec<u32>,
    live: usize,
    limit: usize,
}

impl CapTable {
    /// A table that holds at most `limit` live capabilities. A limit larger
    /// than the index space is clamped to `CapIndex::MAX + 1`.
    pub fn new(limit: usize) -> CapTable {
        let max = CapIndex::MAX.as_usize() + 1;
        CapTable {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            limit: if limit > max { max } else { limit },
        }
    }

    /// The maximum number of live capabilities.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The number of live capabilities.
    pub fn len(&self) -> usize {
        self.live
    }

    /// `true` if no capability is live.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// `true` if `idx` names a live capability.
    pub fn contains(&self, idx: CapIndex) -> bool {
        self.get(idx).is_ok()
    }

    /// Adds a capability and returns its slot.
    ///
    /// Reuses the lowest free slot if there is one, otherwise appends. Fails
    /// with [`CapError::NoMem`] when the table is at its limit (the table is
    /// unchanged and the capability is dropped).
    pub fn insert(&mut self, cap: Capability) -> Result<CapIndex, CapError> {
        if self.live >= self.limit {
            return Err(CapError::NoMem);
        }
        let idx = match self.free.pop() {
            Some(i) => {
                // Invariant 2: a free-list entry is a None slot in range.
                match self.slots.get_mut(i as usize) {
                    Some(slot @ None) => {
                        *slot = Some(cap);
                        CapIndex(i)
                    }
                    // Unreachable by the invariants; treat as corruption
                    // rather than overwrite a live capability.
                    _ => return Err(CapError::NoMem),
                }
            }
            None => {
                // live < limit <= MAX + 1 and no free slots means
                // slots.len() == live, so this index is representable.
                let i = CapIndex::try_from(self.slots.len())?;
                // Fallible growth, as for `Lineage`: `insert` runs on host
                // calls and reports `ENOMEM` instead of aborting the kernel;
                // the free list is kept able to hold every slot so that
                // `drop_slot`/`take` never need to grow it.
                let free_room = (self.slots.len() + 1).saturating_sub(self.free.len());
                if self.slots.try_reserve(1).is_err() || self.free.try_reserve(free_room).is_err() {
                    return Err(CapError::NoMem);
                }
                self.slots.push(Some(cap));
                i
            }
        };
        self.live += 1;
        Ok(idx)
    }

    /// The capability in `idx`, or [`CapError::BadCap`] if the slot is not
    /// live.
    pub fn get(&self, idx: CapIndex) -> Result<&Capability, CapError> {
        self.slots
            .get(idx.as_usize())
            .and_then(Option::as_ref)
            .ok_or(CapError::BadCap)
    }

    /// Looks up `idx` and verifies it is a `kind` object carrying all of
    /// `rights`: [`CapError::BadCap`] if the slot is not live,
    /// [`CapError::Perm`] if the kind or rights do not match.
    pub fn check(
        &self,
        idx: CapIndex,
        kind: ObjectKind,
        rights: Rights,
    ) -> Result<&Capability, CapError> {
        let cap = self.get(idx)?;
        cap.check(kind, rights)?;
        Ok(cap)
    }

    /// Looks up `idx` and verifies it carries all of `rights`, whatever its
    /// kind. Used where the kind is not fixed (`cap_derive`, transfer).
    pub fn check_rights(&self, idx: CapIndex, rights: Rights) -> Result<&Capability, CapError> {
        let cap = self.get(idx)?;
        if cap.rights.contains(rights) {
            Ok(cap)
        } else {
            Err(CapError::Perm)
        }
    }

    /// Removes the capability in `idx` and returns it, freeing the slot.
    /// This is the sender's half of a transfer: the receiving table then
    /// [`insert`](CapTable::insert)s the returned value.
    pub fn take(&mut self, idx: CapIndex) -> Result<Capability, CapError> {
        let slot = self.slots.get_mut(idx.as_usize()).ok_or(CapError::BadCap)?;
        let cap = slot.take().ok_or(CapError::BadCap)?;
        self.live -= 1;
        self.free.push(idx.0);
        Ok(cap)
    }

    /// Mints a weaker capability on the same object into a new slot, per
    /// `cap_derive`, at `child`: a node the object's lineage derived under
    /// the source's node (`Lineage::derive(table.get(idx)?.node)`). See
    /// [`Capability::derive`] for the rights rule. The table does not own
    /// lineages, so on an error the caller releases `child` again; checking
    /// [`derived_rights`](Capability::derived_rights) and the room in the
    /// table (`len() < limit()`) first avoids allocating it for nothing.
    ///
    /// Errors: [`CapError::BadCap`] if `idx` is not live, [`CapError::Perm`]
    /// if the source lacks `DERIVE`, [`CapError::NoMem`] if the table is
    /// full. The source slot is unchanged in every case.
    pub fn derive(
        &mut self,
        idx: CapIndex,
        rights: Rights,
        child: NodeId,
    ) -> Result<CapIndex, CapError> {
        let derived = self.get(idx)?.derive(rights, child)?;
        self.insert(derived)
    }

    /// Removes the capability in `idx`, per `cap_drop`.
    pub fn drop_slot(&mut self, idx: CapIndex) -> Result<(), CapError> {
        self.take(idx).map(drop)
    }

    /// Every live capability with its slot, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (CapIndex, &Capability)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|cap| (CapIndex(i as u32), cap)))
    }

    /// `true` if the table holds at least one live capability of `kind`.
    /// The kernel uses this at instantiation to decide whether
    /// `kalium:spawn` resolves.
    pub fn holds_kind(&self, kind: ObjectKind) -> bool {
        self.iter().any(|(_, cap)| cap.kind == kind)
    }

    /// Checks the structural invariants. Cheap enough for debug assertions;
    /// `true` means consistent.
    pub fn check_invariants(&self) -> bool {
        let live = self.slots.iter().filter(|s| s.is_some()).count();
        if live != self.live || live > self.limit || self.limit > CapIndex::MAX.as_usize() + 1 {
            return false;
        }
        if self.free.len() + live != self.slots.len() {
            return false;
        }
        for (n, &f) in self.free.iter().enumerate() {
            let is_none = matches!(self.slots.get(f as usize), Some(None));
            if !is_none || self.free[..n].contains(&f) {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn console(id: u64) -> Capability {
        Capability::new(
            ObjectId(id),
            ObjectKind::Console,
            Rights::WRITE | Rights::DERIVE,
            NodeId(0),
        )
    }

    fn channel_recv(id: u64) -> Capability {
        Capability::new(
            ObjectId(id),
            ObjectKind::Channel,
            Rights::from_bits(rights::CHANNEL_RECV_END).unwrap(),
            NodeId(0),
        )
    }

    fn memobj(id: u64) -> Capability {
        Capability::new(
            ObjectId(id),
            ObjectKind::MemObj,
            Rights::from_bits(rights::MEMOBJ_OWNER).unwrap(),
            NodeId(0),
        )
    }

    /// A child node for a derivation in tests that do not care about the
    /// lineage: distinct from the helpers' root node 0.
    const CHILD: NodeId = NodeId(1);

    // -- Rights -------------------------------------------------------------

    #[test]
    fn rights_constants_match_abi() {
        assert_eq!(Rights::READ.bits(), rights::READ);
        assert_eq!(Rights::WRITE.bits(), rights::WRITE);
        assert_eq!(Rights::SEND.bits(), rights::SEND);
        assert_eq!(Rights::RECV.bits(), rights::RECV);
        assert_eq!(Rights::DERIVE.bits(), rights::DERIVE);
        assert_eq!(Rights::TRANSFER.bits(), rights::TRANSFER);
        assert_eq!(Rights::KILL.bits(), rights::KILL);
        assert_eq!(Rights::WAIT.bits(), rights::WAIT);
        assert_eq!(Rights::SPAWN.bits(), rights::SPAWN);
        assert_eq!(Rights::SNAPSHOT.bits(), rights::SNAPSHOT);
        assert_eq!(Rights::ALL.bits(), rights::MASK);
        assert_eq!(Rights::NONE, Rights::default());
        assert!(Rights::NONE.is_empty());
        assert!(!Rights::READ.is_empty());
        assert_eq!(u64::from(Rights::READ | Rights::WRITE), 3);
    }

    #[test]
    fn rights_from_bits_rejects_unknown_bits() {
        assert_eq!(Rights::from_bits(0), Some(Rights::NONE));
        assert_eq!(Rights::from_bits(rights::MASK), Some(Rights::ALL));
        assert_eq!(Rights::from_bits(1 << 10), None);
        assert_eq!(Rights::from_bits(u64::MAX), None);
        assert_eq!(Rights::from_bits(rights::READ | (1 << 63)), None);
        assert_eq!(Rights::try_from(1 << 10), Err(CapError::Invalid));
        assert_eq!(Rights::try_from(rights::SEND), Ok(Rights::SEND));
        assert_eq!(Rights::from_bits_truncate(u64::MAX), Rights::ALL);
        assert_eq!(
            Rights::from_bits_truncate(rights::READ | (1 << 40)),
            Rights::READ
        );
    }

    #[test]
    fn rights_set_operations() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(rw.contains(Rights::READ));
        assert!(rw.contains(Rights::WRITE));
        assert!(rw.contains(rw));
        assert!(rw.contains(Rights::NONE));
        assert!(!rw.contains(Rights::SEND));
        assert!(!rw.contains(rw | Rights::SEND));
        assert!(Rights::NONE.contains(Rights::NONE));
        assert!(!Rights::NONE.contains(Rights::READ));

        assert_eq!(rw.intersection(Rights::READ | Rights::SEND), Rights::READ);
        assert_eq!(rw & Rights::SEND, Rights::NONE);
        assert_eq!(
            rw.union(Rights::SEND),
            Rights::READ | Rights::WRITE | Rights::SEND
        );
        assert_eq!(rw.difference(Rights::READ), Rights::WRITE);
        assert_eq!(rw - Rights::WRITE, Rights::READ);
        assert!(rw.intersects(Rights::READ | Rights::KILL));
        assert!(!rw.intersects(Rights::KILL));

        let mut r = Rights::NONE;
        r |= Rights::SPAWN;
        r |= Rights::TRANSFER;
        assert_eq!(r.bits(), rights::INIT_SPAWN);
        r &= Rights::SPAWN;
        assert_eq!(r, Rights::SPAWN);
    }

    #[test]
    fn rights_debug_lists_names() {
        use alloc::format;
        assert_eq!(format!("{:?}", Rights::NONE), "Rights(NONE)");
        assert_eq!(format!("{:?}", Rights::READ), "Rights(READ)");
        assert_eq!(
            format!(
                "{:?}",
                Rights::RECV | Rights::WAIT | Rights::DERIVE | Rights::TRANSFER
            ),
            "Rights(RECV|DERIVE|TRANSFER|WAIT)"
        );
    }

    // -- ObjectKind ---------------------------------------------------------

    #[test]
    fn kind_round_trips_through_u32() {
        for k in ObjectKind::ALL {
            let v: u32 = k.into();
            assert_eq!(ObjectKind::try_from(v), Ok(k));
            assert_eq!(ObjectKind::from_u32(v), Some(k));
            assert_eq!(k.as_u32(), v);
        }
        assert_eq!(u32::from(ObjectKind::Console), 1);
        assert_eq!(u32::from(ObjectKind::Channel), 2);
        assert_eq!(u32::from(ObjectKind::MemObj), 3);
        assert_eq!(u32::from(ObjectKind::Instance), 4);
        assert_eq!(u32::from(ObjectKind::Spawn), 5);
        assert_eq!(u32::from(ObjectKind::Snapshot), 6);
    }

    #[test]
    fn kind_rejects_undefined_values() {
        assert_eq!(ObjectKind::try_from(0), Err(CapError::Invalid));
        assert_eq!(ObjectKind::try_from(7), Err(CapError::Invalid));
        assert_eq!(ObjectKind::try_from(u32::MAX), Err(CapError::Invalid));
        assert_eq!(ObjectKind::from_u32(0), None);
    }

    // -- CapError -----------------------------------------------------------

    #[test]
    fn errors_map_to_errno() {
        assert_eq!(CapError::BadCap.errno(), errno::EBADCAP);
        assert_eq!(CapError::Perm.errno(), errno::EPERM);
        assert_eq!(CapError::NoMem.errno(), errno::ENOMEM);
        assert_eq!(CapError::Invalid.errno(), errno::EINVAL);
        assert_eq!(CapError::BadCap.errno(), 1);
        assert_eq!(CapError::Perm.errno(), 2);
        assert_eq!(CapError::NoMem.errno(), 4);
        assert_eq!(CapError::Invalid.errno(), 3);
        for e in [
            CapError::BadCap,
            CapError::Perm,
            CapError::NoMem,
            CapError::Invalid,
        ] {
            assert_ne!(e.errno(), errno::OK);
            assert!(errno::name(e.errno()).is_some());
        }
    }

    // -- CapIndex -----------------------------------------------------------

    #[test]
    fn index_rejects_negatives() {
        assert_eq!(CapIndex::try_from(-1i32), Err(CapError::BadCap));
        assert_eq!(
            CapIndex::try_from(kalium_abi::CAP_NONE),
            Err(CapError::BadCap)
        );
        assert_eq!(CapIndex::try_from(i32::MIN), Err(CapError::BadCap));
        assert_eq!(CapIndex::try_from(-1000i32), Err(CapError::BadCap));
        assert_eq!(CapIndex::try_from(0i32).map(CapIndex::raw), Ok(0));
        assert_eq!(CapIndex::try_from(7i32).map(CapIndex::raw), Ok(7));
        assert_eq!(CapIndex::try_from(i32::MAX), Ok(CapIndex::MAX));
        assert_eq!(
            CapIndex::try_from(-1i32).unwrap_err().errno(),
            errno::EBADCAP
        );
    }

    #[test]
    fn index_rejects_out_of_range() {
        assert_eq!(CapIndex::new(i32::MAX as u32), Some(CapIndex::MAX));
        assert_eq!(CapIndex::new(i32::MAX as u32 + 1), None);
        assert_eq!(CapIndex::new(u32::MAX), None);
        assert_eq!(CapIndex::try_from(u32::MAX), Err(CapError::BadCap));
        assert_eq!(CapIndex::try_from(usize::MAX), Err(CapError::BadCap));
        assert_eq!(
            CapIndex::try_from(i32::MAX as usize + 1),
            Err(CapError::BadCap)
        );
        assert_eq!(CapIndex::try_from(5usize).map(CapIndex::raw), Ok(5));
    }

    #[test]
    fn index_round_trips_to_abi() {
        for v in [0i32, 1, 42, i32::MAX] {
            let idx = CapIndex::try_from(v).unwrap();
            assert_eq!(idx.as_i32(), v);
            assert_eq!(i32::from(idx), v);
            assert_eq!(u32::from(idx), v as u32);
            assert_eq!(usize::from(idx), v as usize);
        }
    }

    // -- Capability ---------------------------------------------------------

    #[test]
    fn capability_check_and_info() {
        let cap = channel_recv(9);
        assert!(cap.has(Rights::RECV));
        assert!(cap.has(Rights::RECV | Rights::WAIT));
        assert!(!cap.has(Rights::SEND));
        assert_eq!(cap.check(ObjectKind::Channel, Rights::RECV), Ok(()));
        assert_eq!(cap.check(ObjectKind::Channel, Rights::NONE), Ok(()));
        assert_eq!(
            cap.check(ObjectKind::Channel, Rights::SEND),
            Err(CapError::Perm)
        );
        assert_eq!(
            cap.check(ObjectKind::MemObj, Rights::RECV),
            Err(CapError::Perm)
        );
        let info = cap.info();
        assert_eq!(info.kind, kind::CHANNEL);
        assert_eq!(info.rights, rights::CHANNEL_RECV_END);
        assert_eq!(
            kalium_abi::layout::CapInfo::from_le_bytes(info.to_le_bytes()),
            info
        );
    }

    // -- CapTable: insert / get / limits -------------------------------------

    #[test]
    fn insert_and_get() {
        let mut t = CapTable::new(8);
        assert!(t.is_empty());
        assert_eq!(t.limit(), 8);
        let a = t.insert(console(1)).unwrap();
        let b = t.insert(memobj(2)).unwrap();
        assert_eq!(a.raw(), 0);
        assert_eq!(b.raw(), 1);
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
        assert_eq!(t.get(a), Ok(&console(1)));
        assert_eq!(t.get(b), Ok(&memobj(2)));
        assert!(t.contains(a));
        assert!(t.check_invariants());
    }

    #[test]
    fn initial_caps_land_in_slots_in_order() {
        // spawn: "becoming its slots 0..ncaps in order"
        let mut t = CapTable::new(64);
        let caps = [console(1), memobj(2), channel_recv(3)];
        for (i, cap) in caps.iter().enumerate() {
            assert_eq!(t.insert(*cap).unwrap().raw() as usize, i);
        }
        let seen: Vec<_> = t.iter().map(|(i, c)| (i.raw(), *c)).collect();
        assert_eq!(
            seen,
            vec![(0, console(1)), (1, memobj(2)), (2, channel_recv(3))]
        );
    }

    #[test]
    fn limit_gives_enomem() {
        let mut t = CapTable::new(3);
        for i in 0..3 {
            t.insert(console(i)).unwrap();
        }
        let err = t.insert(console(99)).unwrap_err();
        assert_eq!(err, CapError::NoMem);
        assert_eq!(err.errno(), errno::ENOMEM);
        assert_eq!(t.len(), 3);
        // The failed insert did not disturb anything.
        for i in 0..3 {
            assert_eq!(t.get(CapIndex(i)).unwrap().object, ObjectId(i as u64));
        }
        // Freeing a slot makes room for exactly one more.
        t.drop_slot(CapIndex(1)).unwrap();
        assert_eq!(t.insert(console(99)).unwrap().raw(), 1);
        assert_eq!(t.insert(console(100)), Err(CapError::NoMem));
        assert!(t.check_invariants());
    }

    #[test]
    fn zero_limit_table_accepts_nothing() {
        let mut t = CapTable::new(0);
        assert_eq!(t.insert(console(1)), Err(CapError::NoMem));
        assert_eq!(t.get(CapIndex(0)), Err(CapError::BadCap));
        assert!(t.check_invariants());
    }

    #[test]
    fn huge_limit_is_clamped_to_index_space() {
        let t = CapTable::new(usize::MAX);
        assert_eq!(t.limit(), i32::MAX as usize + 1);
        assert!(t.check_invariants());
    }

    #[test]
    fn derive_at_limit_gives_enomem_and_leaves_source() {
        let mut t = CapTable::new(1);
        let a = t.insert(console(1)).unwrap();
        assert_eq!(t.derive(a, Rights::WRITE, CHILD), Err(CapError::NoMem));
        assert_eq!(t.get(a), Ok(&console(1)));
        assert_eq!(t.len(), 1);
    }

    // -- CapTable: bad indices -------------------------------------------------

    #[test]
    fn bad_indices_give_ebadcap() {
        let mut t = CapTable::new(8);
        let a = t.insert(console(1)).unwrap();
        for bad in [CapIndex(1), CapIndex(2), CapIndex(1000), CapIndex::MAX] {
            assert_eq!(t.get(bad), Err(CapError::BadCap));
            assert_eq!(
                t.check(bad, ObjectKind::Console, Rights::NONE),
                Err(CapError::BadCap)
            );
            assert_eq!(t.check_rights(bad, Rights::NONE), Err(CapError::BadCap));
            assert_eq!(t.take(bad), Err(CapError::BadCap));
            assert_eq!(t.derive(bad, Rights::NONE, CHILD), Err(CapError::BadCap));
            assert_eq!(t.drop_slot(bad), Err(CapError::BadCap));
            assert!(!t.contains(bad));
        }
        assert_eq!(CapError::BadCap.errno(), errno::EBADCAP);
        // The live slot is untouched by all of that.
        assert_eq!(t.get(a), Ok(&console(1)));
        assert_eq!(t.len(), 1);
        assert!(t.check_invariants());
    }

    #[test]
    fn freed_slot_is_not_live_until_reinserted() {
        let mut t = CapTable::new(8);
        let a = t.insert(console(1)).unwrap();
        let b = t.insert(memobj(2)).unwrap();
        t.drop_slot(a).unwrap();
        assert_eq!(t.get(a), Err(CapError::BadCap));
        assert_eq!(t.take(a), Err(CapError::BadCap));
        assert_eq!(t.drop_slot(a), Err(CapError::BadCap));
        assert_eq!(t.derive(a, Rights::NONE, CHILD), Err(CapError::BadCap));
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(b), Ok(&memobj(2)));
        assert!(t.check_invariants());
        // Double take through a different path also fails.
        assert_eq!(t.take(b), Ok(memobj(2)));
        assert_eq!(t.take(b), Err(CapError::BadCap));
        assert!(t.is_empty());
        assert!(t.check_invariants());
    }

    #[test]
    fn freed_slots_are_reused_lowest_first() {
        let mut t = CapTable::new(8);
        let idx: Vec<_> = (0..4).map(|i| t.insert(console(i)).unwrap()).collect();
        t.drop_slot(idx[1]).unwrap();
        t.drop_slot(idx[3]).unwrap();
        t.drop_slot(idx[2]).unwrap();
        assert!(t.check_invariants());
        // The free list pops in LIFO order: 2, 3, 1.
        assert_eq!(t.insert(console(10)).unwrap().raw(), 2);
        assert_eq!(t.insert(console(11)).unwrap().raw(), 3);
        assert_eq!(t.insert(console(12)).unwrap().raw(), 1);
        assert_eq!(t.insert(console(13)).unwrap().raw(), 4);
        assert_eq!(t.len(), 5);
        assert!(t.check_invariants());
        // The reused slot holds the new capability, not the old one.
        assert_eq!(t.get(CapIndex(2)).unwrap().object, ObjectId(10));
    }

    // -- CapTable: check (kind / rights) -------------------------------------

    #[test]
    fn check_wrong_kind_gives_eperm() {
        let mut t = CapTable::new(8);
        let a = t.insert(console(1)).unwrap();
        assert_eq!(
            t.check(a, ObjectKind::Console, Rights::WRITE),
            Ok(&console(1))
        );
        let err = t.check(a, ObjectKind::Channel, Rights::WRITE).unwrap_err();
        assert_eq!(err, CapError::Perm);
        assert_eq!(err.errno(), errno::EPERM);
        for k in ObjectKind::ALL {
            if k != ObjectKind::Console {
                assert_eq!(t.check(a, k, Rights::NONE), Err(CapError::Perm));
            }
        }
    }

    #[test]
    fn check_missing_rights_gives_eperm() {
        let mut t = CapTable::new(8);
        let a = t.insert(console(1)).unwrap(); // WRITE|DERIVE
        assert!(t.check(a, ObjectKind::Console, Rights::NONE).is_ok());
        assert!(t.check(a, ObjectKind::Console, Rights::WRITE).is_ok());
        assert!(t
            .check(a, ObjectKind::Console, Rights::WRITE | Rights::DERIVE)
            .is_ok());
        assert_eq!(
            t.check(a, ObjectKind::Console, Rights::READ),
            Err(CapError::Perm)
        );
        assert_eq!(
            t.check(a, ObjectKind::Console, Rights::WRITE | Rights::TRANSFER),
            Err(CapError::Perm)
        );
        assert_eq!(t.check_rights(a, Rights::WRITE), Ok(&console(1)));
        assert_eq!(t.check_rights(a, Rights::TRANSFER), Err(CapError::Perm));
        // A bad index wins over a rights problem.
        assert_eq!(
            t.check(CapIndex(5), ObjectKind::Console, Rights::READ),
            Err(CapError::BadCap)
        );
    }

    #[test]
    fn holds_kind_reflects_live_caps() {
        let mut t = CapTable::new(8);
        assert!(!t.holds_kind(ObjectKind::Spawn));
        let s = t
            .insert(Capability::new(
                ObjectId(7),
                ObjectKind::Spawn,
                Rights::SPAWN | Rights::TRANSFER,
                NodeId(0),
            ))
            .unwrap();
        assert!(t.holds_kind(ObjectKind::Spawn));
        assert!(!t.holds_kind(ObjectKind::Console));
        t.take(s).unwrap();
        assert!(!t.holds_kind(ObjectKind::Spawn));
    }

    // -- CapTable: transfer via take ------------------------------------------

    #[test]
    fn transfer_moves_capability_between_tables() {
        let mut sender = CapTable::new(8);
        let mut receiver = CapTable::new(8);
        let keep = sender.insert(console(1)).unwrap();
        let cap = memobj(2);
        let idx = sender.insert(cap).unwrap();

        // The sender must hold TRANSFER on what it sends.
        sender.check_rights(idx, Rights::TRANSFER).unwrap();
        let moved = sender.take(idx).unwrap();
        assert_eq!(moved, cap);
        assert_eq!(sender.get(idx), Err(CapError::BadCap));
        assert_eq!(sender.len(), 1);
        assert_eq!(sender.get(keep), Ok(&console(1)));

        let new_idx = receiver.insert(moved).unwrap();
        assert_eq!(new_idx.raw(), 0);
        assert_eq!(receiver.get(new_idx), Ok(&cap));
        // Same object, same rights: the capability is unchanged by transfer.
        assert_eq!(receiver.get(new_idx).unwrap().object, ObjectId(2));
        assert!(sender.check_invariants() && receiver.check_invariants());
    }

    #[test]
    fn transfer_without_transfer_right_is_refused_by_check() {
        let mut t = CapTable::new(8);
        let a = t.insert(console(1)).unwrap(); // no TRANSFER
        assert_eq!(t.check_rights(a, Rights::TRANSFER), Err(CapError::Perm));
        // take itself is policy-free; the kernel checks first.
        assert_eq!(t.take(a), Ok(console(1)));
    }

    #[test]
    fn transfer_into_full_table_fails_and_cap_is_returned_to_caller() {
        let mut sender = CapTable::new(8);
        let mut receiver = CapTable::new(0);
        let idx = sender.insert(memobj(2)).unwrap();
        let moved = sender.take(idx).unwrap();
        assert_eq!(receiver.insert(moved), Err(CapError::NoMem));
        // The kernel can put it back in the sender.
        let back = sender.insert(moved).unwrap();
        assert_eq!(back, idx);
        assert_eq!(sender.get(back), Ok(&memobj(2)));
    }

    // -- CapTable: derive -------------------------------------------------------

    #[test]
    fn derive_intersects_rights() {
        let mut t = CapTable::new(8);
        let a = t.insert(memobj(1)).unwrap(); // READ|WRITE|DERIVE|TRANSFER
        let d = t.derive(a, Rights::READ, CHILD).unwrap();
        assert_ne!(d, a);
        let derived = *t.get(d).unwrap();
        assert_eq!(derived.object, ObjectId(1));
        assert_eq!(derived.kind, ObjectKind::MemObj);
        assert_eq!(derived.rights, Rights::READ);
        assert_eq!(derived.node, CHILD);
        // The source is unchanged.
        assert_eq!(t.get(a), Ok(&memobj(1)));
        assert_eq!(t.len(), 2);
        assert!(t.check_invariants());
    }

    #[test]
    fn derive_never_gains_rights() {
        let mut t = CapTable::new(8);
        let a = t.insert(memobj(1)).unwrap(); // READ|WRITE|DERIVE|TRANSFER
        let d = t.derive(a, Rights::ALL, CHILD).unwrap();
        assert_eq!(t.get(d).unwrap().rights, t.get(a).unwrap().rights);
        let d2 = t
            .derive(a, Rights::READ | Rights::KILL | Rights::SPAWN, CHILD)
            .unwrap();
        assert_eq!(t.get(d2).unwrap().rights, Rights::READ);
        // Exhaustively: for every requested subset, derived <= source.
        for bits in 0..=rights::MASK {
            let req = Rights::from_bits(bits).unwrap();
            let src = *t.get(a).unwrap();
            let derived = src.derive(req, CHILD).unwrap();
            assert!(src.rights.contains(derived.rights));
            assert!(req.contains(derived.rights));
            assert_eq!(derived.rights, src.rights & req);
            assert_eq!(src.derived_rights(req), Ok(derived.rights));
            assert_eq!(derived.object, src.object);
            assert_eq!(derived.kind, src.kind);
            assert_eq!(derived.node, CHILD);
        }
    }

    #[test]
    fn derive_requires_derive_right() {
        let mut t = CapTable::new(8);
        let a = t
            .insert(Capability::new(
                ObjectId(1),
                ObjectKind::MemObj,
                Rights::READ | Rights::WRITE | Rights::TRANSFER,
                NodeId(0),
            ))
            .unwrap();
        let err = t.derive(a, Rights::READ, CHILD).unwrap_err();
        assert_eq!(err, CapError::Perm);
        assert_eq!(err.errno(), errno::EPERM);
        // Even asking for nothing needs DERIVE.
        assert_eq!(t.derive(a, Rights::NONE, CHILD), Err(CapError::Perm));
        assert_eq!(
            t.get(a).unwrap().derived_rights(Rights::NONE),
            Err(CapError::Perm)
        );
        assert_eq!(t.len(), 1);
        assert!(t.check_invariants());
    }

    #[test]
    fn derived_cap_keeps_derive_only_when_requested() {
        let mut t = CapTable::new(8);
        let a = t.insert(memobj(1)).unwrap();
        // Not requested: the derived cap cannot derive further.
        let d = t.derive(a, Rights::READ, NodeId(1)).unwrap();
        assert!(!t.get(d).unwrap().has(Rights::DERIVE));
        assert_eq!(t.derive(d, Rights::READ, NodeId(2)), Err(CapError::Perm));
        // Requested: it can, but still only within its own rights.
        let d2 = t
            .derive(a, Rights::READ | Rights::DERIVE, NodeId(2))
            .unwrap();
        assert!(t.get(d2).unwrap().has(Rights::DERIVE));
        let d3 = t
            .derive(d2, Rights::READ | Rights::WRITE | Rights::DERIVE, NodeId(3))
            .unwrap();
        assert_eq!(t.get(d3).unwrap().rights, Rights::READ | Rights::DERIVE);
        let d4 = t
            .derive(d3, Rights::ALL - Rights::DERIVE, NodeId(4))
            .unwrap();
        assert_eq!(t.get(d4).unwrap().rights, Rights::READ);
        assert_eq!(t.derive(d4, Rights::ALL, NodeId(5)), Err(CapError::Perm));
    }

    #[test]
    fn derive_chain_is_monotonically_weaker() {
        let mut t = CapTable::new(64);
        let mut idx = t.insert(channel_recv(5)).unwrap();
        let mut prev = *t.get(idx).unwrap();
        // Peel off one right per step, keeping DERIVE until the end.
        for (n, strip) in [Rights::TRANSFER, Rights::WAIT, Rights::RECV, Rights::DERIVE]
            .into_iter()
            .enumerate()
        {
            idx = t
                .derive(idx, prev.rights - strip, NodeId(n as u32 + 1))
                .unwrap();
            let cur = *t.get(idx).unwrap();
            assert!(prev.rights.contains(cur.rights));
            assert!(!cur.has(strip));
            assert_eq!(cur.object, ObjectId(5));
            prev = cur;
        }
        assert_eq!(prev.rights, Rights::NONE);
        assert!(t.check_invariants());
    }

    #[test]
    fn derived_and_source_are_independent_slots() {
        let mut t = CapTable::new(8);
        let a = t.insert(memobj(1)).unwrap();
        let d = t.derive(a, Rights::READ, CHILD).unwrap();
        t.drop_slot(a).unwrap();
        assert_eq!(t.get(a), Err(CapError::BadCap));
        assert_eq!(t.get(d).unwrap().rights, Rights::READ);
        t.drop_slot(d).unwrap();
        assert!(t.is_empty());
        assert!(t.check_invariants());
    }

    // -- CapTable: structural ---------------------------------------------------

    #[test]
    fn churn_keeps_invariants() {
        // A deterministic pseudo-random sequence of inserts, derives, takes
        // and drops, checking the invariants and a shadow model throughout.
        let mut t = CapTable::new(16);
        let mut shadow: Vec<Option<Capability>> = Vec::new();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for step in 0..5000u64 {
            let r = next();
            let slot = (r >> 8) as usize % 20;
            let idx = CapIndex(slot as u32);
            match r % 4 {
                0 => {
                    let cap = memobj(step);
                    match t.insert(cap) {
                        Ok(i) => {
                            let i = i.raw() as usize;
                            if i >= shadow.len() {
                                shadow.resize(i + 1, None);
                            }
                            assert_eq!(shadow[i], None);
                            shadow[i] = Some(cap);
                        }
                        Err(CapError::NoMem) => {
                            assert_eq!(shadow.iter().flatten().count(), 16);
                        }
                        Err(e) => panic!("unexpected {e:?}"),
                    }
                }
                1 => {
                    let expected = shadow.get(slot).copied().flatten();
                    assert_eq!(t.take(idx).ok(), expected);
                    if slot < shadow.len() {
                        shadow[slot] = None;
                    }
                }
                2 => {
                    let expected = shadow.get(slot).copied().flatten();
                    assert_eq!(t.drop_slot(idx).is_ok(), expected.is_some());
                    if slot < shadow.len() {
                        shadow[slot] = None;
                    }
                }
                _ => {
                    let req = Rights::from_bits_truncate(r >> 32);
                    let src = shadow.get(slot).copied().flatten();
                    match (t.derive(idx, req, CHILD), src) {
                        (Ok(i), Some(src)) => {
                            assert!(src.has(Rights::DERIVE));
                            let i = i.raw() as usize;
                            if i >= shadow.len() {
                                shadow.resize(i + 1, None);
                            }
                            assert_eq!(shadow[i], None);
                            shadow[i] = Some(src.derive(req, CHILD).unwrap());
                        }
                        (Err(CapError::BadCap), None) => {}
                        (Err(CapError::Perm), Some(src)) => {
                            assert!(!src.has(Rights::DERIVE));
                        }
                        (Err(CapError::NoMem), Some(src)) => {
                            assert!(src.has(Rights::DERIVE));
                            assert_eq!(shadow.iter().flatten().count(), 16);
                        }
                        (res, src) => panic!("derive {res:?} vs shadow {src:?}"),
                    }
                }
            }
            assert!(t.check_invariants(), "invariants broken at step {step}");
            let live: Vec<_> = t.iter().map(|(i, c)| (i.raw() as usize, *c)).collect();
            let expected: Vec<_> = shadow
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|c| (i, c)))
                .collect();
            assert_eq!(live, expected);
            assert_eq!(t.len(), expected.len());
        }
    }

    // -- Lineage ------------------------------------------------------------

    /// A root with one capability on it.
    fn rooted(l: &mut Lineage) -> NodeId {
        let n = l.root().unwrap();
        l.retain(n).unwrap();
        n
    }

    /// A child of `parent` with one capability on it.
    fn child_of(l: &mut Lineage, parent: NodeId) -> NodeId {
        let n = l.derive(parent).unwrap();
        l.retain(n).unwrap();
        n
    }

    #[test]
    fn lineage_depth_limit_matches_abi() {
        assert_eq!(MAX_DEPTH, limits::MAX_DERIVE_DEPTH);
        assert_eq!(MAX_DEPTH, 16);
    }

    #[test]
    fn derive_chain_is_valid_until_revoked() {
        let mut l = Lineage::new();
        assert!(l.is_empty());
        let r = rooted(&mut l);
        let a = child_of(&mut l, r);
        let b = child_of(&mut l, a);
        let c = child_of(&mut l, b);
        assert_eq!(l.len(), 4);
        assert_eq!(l.depth(r), Some(0));
        assert_eq!(l.depth(c), Some(3));
        assert_eq!(l.parent(c), Some(b));
        assert_eq!(l.parent(r), None);
        for n in [r, a, b, c] {
            assert!(l.is_valid(n));
            assert!(l.is_descendant(n, r));
        }
        assert!(!l.is_descendant(r, a));
        assert!(l.check_invariants());

        // Revoking the middle: everything below it dies, it and what is
        // above it live on.
        l.revoke(a).unwrap();
        assert!(l.is_valid(r));
        assert!(l.is_valid(a));
        assert!(!l.is_valid(b));
        assert!(!l.is_valid(c));
        // Dead nodes are still live nodes (their capabilities still exist).
        assert_eq!(l.len(), 4);
        assert!(l.contains(c));
        assert!(l.check_invariants());
    }

    #[test]
    fn revoke_hits_the_subtree_only() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        let a = child_of(&mut l, r);
        let a1 = child_of(&mut l, a);
        let a2 = child_of(&mut l, a);
        let b = child_of(&mut l, r); // a's sibling
        let b1 = child_of(&mut l, b);
        let other_root = rooted(&mut l); // another fresh capability on the object

        l.revoke(a).unwrap();
        assert!(!l.is_valid(a1));
        assert!(!l.is_valid(a2));
        for n in [r, a, b, b1, other_root] {
            assert!(l.is_valid(n), "{n:?} must survive revoking a sibling/child");
        }
        // Revoking a1 (already dead) changes nothing and is not an error.
        assert_eq!(l.revoke(a1), Ok(()));
        assert!(!l.is_valid(a1));
        // Revoking the root kills every descendant but not the other root.
        l.revoke(r).unwrap();
        assert!(l.is_valid(r));
        assert!(!l.is_valid(a));
        assert!(!l.is_valid(b));
        assert!(!l.is_valid(b1));
        assert!(l.is_valid(other_root));
        assert!(l.check_invariants());
    }

    #[test]
    fn revoked_node_can_derive_again() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        let old = child_of(&mut l, r);
        l.revoke(r).unwrap();
        assert!(!l.is_valid(old));
        let new = child_of(&mut l, r);
        assert!(l.is_valid(new));
        assert!(!l.is_valid(old));
        let grandchild = child_of(&mut l, new);
        assert!(l.is_valid(grandchild));
        // And a second revocation kills the new ones too.
        l.revoke(r).unwrap();
        assert!(!l.is_valid(new));
        assert!(!l.is_valid(grandchild));
        assert!(l.check_invariants());
    }

    #[test]
    fn depth_limit_gives_enomem() {
        let mut l = Lineage::new();
        let mut n = rooted(&mut l);
        for d in 1..=MAX_DEPTH {
            n = child_of(&mut l, n);
            assert_eq!(l.depth(n), Some(d));
        }
        let err = l.derive(n).unwrap_err();
        assert_eq!(err, CapError::NoMem);
        assert_eq!(err.errno(), errno::ENOMEM);
        // The leaf is still valid and the failed derive left no node.
        assert!(l.is_valid(n));
        assert_eq!(l.len(), MAX_DEPTH as usize + 1);
        assert!(l.check_invariants());
        // The walk from the deepest leaf still terminates with the answer.
        let root = NodeId(0);
        l.revoke(root).unwrap();
        assert!(!l.is_valid(n));
        assert!(l.is_valid(root));
    }

    /// M4-3: a revocation bumps the node's 32-bit generation; once it
    /// would wrap, `revoke` and `revoke_among` fail with `NoMem` and
    /// invalidate nothing, so the kernel must report the error rather
    /// than claim a successful revocation (`Kernel::revoke`, `cap_revoke`).
    #[test]
    fn generation_wrap_gives_enomem_and_invalidates_nothing() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        let a = child_of(&mut l, r);
        // 2^32 - 1 revocations of `a` would take too long: jump to the
        // state just before the last one that succeeds.
        l.get_mut(a).unwrap().generation = u32::MAX - 1;
        let before = child_of(&mut l, a);
        assert_eq!(l.can_revoke(a), Ok(()));
        assert_eq!(l.revoke(a), Ok(()));
        assert!(!l.is_valid(before));
        assert_eq!(l.get(a).unwrap().generation, u32::MAX);

        // The next revocation cannot be recorded: an error, and the child
        // derived meanwhile stays valid (it is exactly what the kernel's
        // caller is told was invalidated if the error is swallowed).
        // `can_revoke` says so beforehand, and changes nothing.
        let after = child_of(&mut l, a);
        assert_eq!(l.can_revoke(a), Err(CapError::NoMem));
        assert_eq!(l.can_revoke(r), Ok(()));
        assert_eq!(l.can_revoke(NodeId(99)), Err(CapError::BadCap));
        assert!(l.is_valid(after));
        let err = l.revoke(a).unwrap_err();
        assert_eq!(err, CapError::NoMem);
        assert_eq!(err.errno(), errno::ENOMEM);
        assert!(l.is_valid(after));
        assert_eq!(l.get(a).unwrap().generation, u32::MAX);

        let cap = |node| Capability {
            object: ObjectId(1),
            kind: ObjectKind::Channel,
            rights: Rights::SEND,
            node,
        };
        let held = [cap(r), cap(a), cap(before), cap(after)];
        assert_eq!(l.revoke_among(a, &held), Err(CapError::NoMem));
        assert!(l.is_valid(r));
        assert!(l.is_valid(a));
        assert!(!l.is_valid(before));
        assert!(l.is_valid(after));
        assert!(l.check_invariants());

        // `a` can still derive, and an ancestor's revocation still cuts
        // off the whole chain below it, `after` included.
        let late = child_of(&mut l, a);
        assert!(l.is_valid(late));
        let dead = l.revoke_among(r, &held).unwrap();
        assert_eq!(dead, vec![cap(a), cap(after)]);
        assert!(l.is_valid(r));
        assert!(!l.is_valid(a));
        assert!(!l.is_valid(after));
        assert!(!l.is_valid(late));
        assert!(l.check_invariants());
    }

    #[test]
    fn unknown_nodes_are_rejected() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        let bad = NodeId(7);
        assert!(!l.is_valid(bad));
        assert!(!l.contains(bad));
        assert_eq!(l.depth(bad), None);
        assert_eq!(l.derive(bad), Err(CapError::BadCap));
        assert_eq!(l.retain(bad), Err(CapError::BadCap));
        assert_eq!(l.revoke(bad), Err(CapError::BadCap));
        assert!(!l.is_descendant(bad, r));
        assert!(!l.is_descendant(r, bad));
        l.release(bad); // ignored
        assert_eq!(l.len(), 1);
        assert!(l.is_valid(r));
        assert!(l.check_invariants());
    }

    #[test]
    fn transfer_keeps_the_node() {
        // A derived capability moved to another table is revoked with the
        // chain it came from, because the value carries its node.
        let mut l = Lineage::new();
        let mut init = CapTable::new(8);
        let mut child = CapTable::new(8);
        let root = l.root().unwrap();
        let orig = init.insert(console(1).with_node(root)).unwrap();
        l.retain(root).unwrap();
        let node = l.derive(root).unwrap();
        let d = init.derive(orig, Rights::WRITE, node).unwrap();
        l.retain(node).unwrap();

        let moved = init.take(d).unwrap();
        let in_child = child.insert(moved).unwrap();
        assert_eq!(child.get(in_child).unwrap().node, node);
        assert!(l.is_valid(child.get(in_child).unwrap().node));

        l.revoke(init.get(orig).unwrap().node).unwrap();
        assert!(!l.is_valid(child.get(in_child).unwrap().node));
        assert!(l.is_valid(init.get(orig).unwrap().node));
        // The dead capability still occupies its slot until dropped.
        assert!(child.contains(in_child));
        child.drop_slot(in_child).unwrap();
        l.release(node);
        assert_eq!(l.len(), 1);
        assert!(l.check_invariants());
    }

    #[test]
    fn nodes_are_freed_when_unreferenced() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        // A derive/drop loop must not grow the tree.
        for _ in 0..10_000 {
            let c = child_of(&mut l, r);
            l.release(c);
            assert_eq!(l.len(), 1);
        }
        // Dropping a parent keeps it alive while a child needs it.
        let a = child_of(&mut l, r);
        let b = child_of(&mut l, a);
        l.release(a);
        assert_eq!(l.len(), 3, "a parent with a live child stays");
        assert_eq!(l.refs(a), Some(0));
        assert!(l.is_valid(b));
        // Dropping a parent is not revoking: b lives on.
        l.release(b);
        assert_eq!(l.len(), 1, "freeing b frees the unreferenced a as well");
        assert!(!l.contains(a));
        assert!(!l.contains(b));
        assert!(l.is_valid(r));
        // Two capabilities on one node: freed with the last.
        let shared = child_of(&mut l, r);
        l.retain(shared).unwrap();
        l.release(shared);
        assert!(l.contains(shared));
        l.release(shared);
        assert!(!l.contains(shared));
        // The root itself.
        l.release(r);
        assert!(l.is_empty());
        assert!(l.check_invariants());
        // A node returned by root()/derive() without a retain is released
        // by a single release (the mint failed).
        let n = l.root().unwrap();
        assert_eq!(l.refs(n), Some(0));
        l.release(n);
        assert!(l.is_empty());
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        let a = child_of(&mut l, r);
        l.release(a);
        let b = child_of(&mut l, r);
        assert_eq!(a, b, "the freed slot is reused");
        // The reused node is a fresh, valid child even though the old one
        // at this index was revoked.
        l.revoke(r).unwrap();
        assert!(!l.is_valid(b));
        l.release(b);
        let c = child_of(&mut l, r);
        assert_eq!(c, b);
        assert!(l.is_valid(c));
        assert!(l.check_invariants());
    }

    #[test]
    fn revoke_among_reports_transitions_only() {
        let mut l = Lineage::new();
        let r = rooted(&mut l);
        let a = child_of(&mut l, r);
        let a1 = child_of(&mut l, a);
        let b = child_of(&mut l, r);
        let caps = [
            channel_recv(1).with_node(r),
            channel_recv(1).with_node(a),
            channel_recv(1).with_node(a1),
            channel_recv(1).with_node(b),
        ];
        // Revoking a: only a1 transitions.
        let dead = l.revoke_among(a, &caps).unwrap();
        assert_eq!(dead, vec![caps[2]]);
        // Revoking the root: a and b transition; a1 was already dead and
        // is not reported again.
        let dead = l.revoke_among(r, &caps).unwrap();
        assert_eq!(dead, vec![caps[1], caps[3]]);
        // Nothing left to transition.
        assert_eq!(l.revoke_among(r, &caps).unwrap(), vec![]);
        assert_eq!(l.revoke_among(NodeId(9), &caps), Err(CapError::BadCap));
    }
}

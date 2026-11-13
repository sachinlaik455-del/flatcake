//! tests/gva_tests.rs — FC-GVA integration tests (T1–T6 + arena).
//!
//! The crate is lib+bin, so these tests link `fckv` as a library. The SSD
//! layout is a std-only `SsdLayout` stand-in (`FakeLayout`) because the real
//! `cio::layout` is developed in parallel against the same frozen trait; no
//! filesystem or `src/cio` dependency is needed.

use fckv::config::Config;
use fckv::gva::arena::Arena;
use fckv::gva::index::Index;
use fckv::gva::placement::decide;
use fckv::gva::reclaim::{reclaim_pass, ReclaimStore, ScopeRegistry};
use fckv::types::{
    BlockKey, DramArena, FileRange, Gva, Lookup, Medium, Metrics, PlacementSignals, SsdLayout,
    ArenaFull, MEDIA_DRAM, MEDIA_SSD, SSD_FILE_MAX, SSD_IN_FILE_BITS,
};
use fckv::util::key_hash;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};

// ---------------------------------------------------------------- helpers

fn key(session: u64, layer: u32, token_start: u64) -> BlockKey {
    BlockKey {
        session,
        layer,
        token_start,
    }
}

fn dram_gva(off: u64) -> Gva {
    Gva::encode(MEDIA_DRAM, 0, off).expect("valid DRAM GVA")
}

fn ssd_gva(file_seq: u32, in_file_off: u64) -> Gva {
    Gva::encode(MEDIA_SSD, 0, ((file_seq as u64) << SSD_IN_FILE_BITS) | in_file_off)
        .expect("valid SSD GVA")
}

fn sig(session: u64, refcount: u32, turn: u64, reply: u64, ctx: u64) -> PlacementSignals {
    PlacementSignals {
        session,
        refcount,
        turn,
        reply_tokens: reply,
        ctx_tokens: ctx,
    }
}

fn publish_key(index: &Index, k: &BlockKey, g: Gva) -> usize {
    let slot = index.insert_pending(k).expect("index slot available");
    assert!(index.publish(slot, g), "publish of fresh PENDING slot");
    slot
}

/// Brute-force a key whose probe STARTS exactly at `slot` — lets the collision
/// tests build deterministic linear-probe chains.
fn find_key_starting_at(session: u64, layer: u32, slot: usize, cap: u64) -> BlockKey {
    for t in 0..1_000_000u64 {
        let k = key(session, layer, t * 8192 + 4096);
        if key_hash(k.session, k.layer, k.token_start) % cap == slot as u64 {
            return k;
        }
    }
    panic!("no key hashes to slot {slot} within search budget (unexpected)");
}

/// Minimal std-only SSD layout stand-in (real one is `cio::layout`).
struct FakeLayout {
    freed: Mutex<Vec<(u32, u64, u64)>>,
    file_cap: u64,
}

impl SsdLayout for FakeLayout {
    fn resolve(&self, gva: Gva, len: u64) -> FileRange {
        FileRange {
            file_seq: (gva.offset() >> SSD_IN_FILE_BITS) as u32,
            in_file_off: gva.offset() & (SSD_FILE_MAX - 1),
            len,
        }
    }
    fn free_extent(&self, file_seq: u32, off: u64, len: u64) {
        self.freed.lock().unwrap().push((file_seq, off, len));
    }
    fn logical_bytes(&self) -> u64 {
        0 // usage is caller-supplied in these tests
    }
    fn file_capacity(&self) -> u64 {
        self.file_cap
    }
}

/// `DramArena` wrapper asserting the D15 ordering: by the time any extent of
/// a key is freed, that key must already be tombstoned (not Hit). Also logs
/// the free order for oldest-first assertions.
struct SpyArena<'a> {
    inner: Arena,
    index: &'a Index,
    off2key: Mutex<HashMap<u64, BlockKey>>,
    freed_order: Mutex<Vec<u64>>,
}

impl SpyArena<'_> {
    fn register(&self, off: u64, k: BlockKey) {
        self.off2key.lock().unwrap().insert(off, k);
    }
}

impl DramArena for SpyArena<'_> {
    fn reserve(&self, len: u64) -> Result<u64, ArenaFull> {
        self.inner.reserve(len)
    }
    fn write(&self, off: u64, buf: &[u8]) {
        self.inner.write(off, buf)
    }
    fn read(&self, off: u64, len: u64, out: &mut [u8]) {
        self.inner.read(off, len, out)
    }
    fn free_extent(&self, off: u64, len: u64) {
        if let Some(k) = self.off2key.lock().unwrap().get(&off) {
            assert!(
                !matches!(self.index.lookup(k), Lookup::Hit(_)),
                "D15 violation: extent for {k:?} freed while still published"
            );
        }
        self.freed_order.lock().unwrap().push(off);
        self.inner.free_extent(off, len);
    }
    fn used_bytes(&self) -> u64 {
        self.inner.used_bytes()
    }
    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
}

/// Build a DRAM scope with `n_blocks` published 1024-byte blocks.
fn make_dram_scope(
    spy: &SpyArena<'_>,
    index: &Index,
    reg: &ScopeRegistry,
    session: u64,
    ts: u64,
    n_blocks: u64,
) -> Vec<BlockKey> {
    reg.create_scope(session, Medium::Dram, ts);
    let mut keys = Vec::new();
    for i in 0..n_blocks {
        let k = key(session, 0, i * 256);
        let off = spy.reserve(1024).expect("scope extent fits");
        let mut buf = vec![0u8; 1024];
        buf.fill((session % 251) as u8);
        spy.write(off, &buf);
        publish_key(index, &k, dram_gva(off));
        spy.register(off, k);
        assert!(reg.add_dram_extent(session, off, 1024));
        assert!(reg.add_key(session, k));
        keys.push(k);
    }
    keys
}

// ==================================================================== T1

#[test]
fn t1_gva_round_trip_and_boundaries() {
    for &(m, node, off) in &[
        (MEDIA_DRAM, 0u64, 0u64),
        (MEDIA_DRAM, 4095u64, 0x0000_FFFF_FFFF_FFFFu64),
        (MEDIA_SSD, 0u64, 0u64),
        (MEDIA_SSD, 4095u64, 0x0000_FFFF_FFFF_FFFFu64),
        (MEDIA_SSD, 1u64, 1u64 << SSD_IN_FILE_BITS),
    ] {
        let g = Gva::encode(m, node, off).expect("encode must accept in-range fields");
        assert_eq!(g.media(), m, "media round-trip");
        assert_eq!(g.node(), node, "node round-trip");
        assert_eq!(g.offset(), off, "offset round-trip");
    }
}

#[test]
fn t1_gva_invalid_values_rejected() {
    assert!(Gva::encode(2, 0, 0).is_none(), "media >= 2 reserved");
    assert!(Gva::encode(u64::MAX, 0, 0).is_none(), "media >= 2 reserved");
    assert!(Gva::encode(0, 0x1000, 0).is_none(), "node is 12 bits");
    assert!(Gva::encode(1, 0xFFF, 1 << 48).is_none(), "offset is 48 bits");
}

#[test]
fn t1_gva_dram_ssd_classification_and_ssd_packing() {
    let d = Gva::encode(MEDIA_DRAM, 0, (1 << 48) - 1).unwrap();
    assert!(!d.is_ssd(), "DRAM GVA < 2^60");
    let s = Gva::encode(MEDIA_SSD, 0, 0).unwrap();
    assert!(s.is_ssd(), "SSD GVA >= 2^60");
    // SSD offset packing (ARCHITECTURE §6): off[47:0] = file_seq[47:28] | in_file_off[27:0]
    let (fs, in_off) = (3u64, 5000u64);
    let g = Gva::encode(MEDIA_SSD, 0, (fs << SSD_IN_FILE_BITS) | in_off).unwrap();
    assert_eq!(g.offset() >> SSD_IN_FILE_BITS, fs);
    assert_eq!(g.offset() & (SSD_FILE_MAX - 1), in_off);
}

// ==================================================================== T2

#[test]
fn t2_index_lookup_hit_pending_miss() {
    let idx = Index::from_config(&Config::default());
    let k = key(7, 1, 256);
    assert!(matches!(idx.lookup(&k), Lookup::Miss), "uninserted key misses");
    let slot = idx.insert_pending(&k).expect("insert into empty table");
    assert!(matches!(idx.lookup(&k), Lookup::Pending), "PENDING visible as Pending");
    let g = dram_gva(1234);
    assert!(idx.publish(slot, g));
    match idx.lookup(&k) {
        Lookup::Hit(h) => assert_eq!(h, g),
        other => panic!("expected Hit after publish, got {other:?}"),
    }
    assert!(idx.invalidate(&k));
    assert!(matches!(idx.lookup(&k), Lookup::Miss), "invalidated key misses");
}

#[test]
fn t2_index_duplicate_key_returns_same_slot_idempotently() {
    let idx = Index::new(256);
    let k = key(1, 0, 0);
    let s1 = idx.insert_pending(&k).expect("first insert");
    let s2 = idx.insert_pending(&k).expect("duplicate while PENDING");
    assert_eq!(s1, s2, "duplicate identity while PENDING");
    assert!(idx.publish(s1, dram_gva(64)));
    let s3 = idx.insert_pending(&k).expect("duplicate while PUBLISHED");
    assert_eq!(s1, s3, "duplicate identity while PUBLISHED");
    assert!(!idx.publish(s1, dram_gva(65)), "double publish rejected");
    match idx.lookup(&k) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 64, "first publish wins"),
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn t2_index_linear_probe_collision_chain() {
    let cap = 64u64;
    let idx = Index::new(cap as u64);
    let ka = key(77, 0, 0);
    let sa = idx.insert_pending(&ka).expect("a");
    idx.publish(sa, dram_gva(10));
    // kb's probe starts exactly at ka's slot; kc's starts at kb's slot.
    let kb = find_key_starting_at(77, 7, sa, cap);
    let sb = idx.insert_pending(&kb).expect("b");
    assert_eq!(sb, (sa + 1) % 64, "kb claims the slot after ka");
    idx.publish(sb, dram_gva(20));
    let kc = find_key_starting_at(77, 7, sb, cap);
    let sc = idx.insert_pending(&kc).expect("c");
    assert_eq!(sc, (sa + 2) % 64, "kc claims the slot after kb");
    idx.publish(sc, dram_gva(30));
    // all three resolve despite the shared probe chain
    match idx.lookup(&ka) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 10),
        other => panic!("ka lost: {other:?}"),
    }
    match idx.lookup(&kb) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 20),
        other => panic!("kb lost: {other:?}"),
    }
    match idx.lookup(&kc) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 30),
        other => panic!("kc lost: {other:?}"),
    }
    // tombstone the middle of the chain: lookups must traverse past it
    assert!(idx.invalidate(&kb));
    assert!(matches!(idx.lookup(&kb), Lookup::Miss));
    match idx.lookup(&kc) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 30),
        other => panic!("kc unreachable past tombstone: {other:?}"),
    }
}

#[test]
fn t2_index_tombstone_slot_reusable() {
    let cap = 64u64;
    let idx = Index::new(cap);
    let ka = key(9, 0, 0);
    let sa = idx.insert_pending(&ka).expect("a");
    idx.publish(sa, dram_gva(10));
    // find a second key probing the same start (collision)
    let kb = find_key_starting_at(9, 5, sa, cap);
    let sb = idx.insert_pending(&kb).expect("b lands after a");
    idx.publish(sb, dram_gva(20));
    // invalidate BOTH: chain now two tombstones; a new insert may reuse them
    assert!(idx.invalidate(&ka));
    assert!(idx.invalidate(&kb));
    assert!(matches!(idx.lookup(&ka), Lookup::Miss));
    assert!(matches!(idx.lookup(&kb), Lookup::Miss));
    let kc = find_key_starting_at(9, 6, sa, cap);
    let sc = idx.insert_pending(&kc).expect("tombstone slots reusable by inserts");
    idx.publish(sc, dram_gva(30));
    match idx.lookup(&kc) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 30),
        other => panic!("kc lost: {other:?}"),
    }
}

#[test]
fn t2_index_concurrent_stress_no_lost_publishes() {
    const INSERTERS: usize = 8;
    const PER_INSERTER: usize = 700; // 5600 keys / 8192 slots = 0.68 load <= 0.7 (OQ-16)
    const LOOKERS: usize = 4;
    let idx = Arc::new(Index::new((INSERTERS * PER_INSERTER * 10 / 7) as u64));
    let mut expected = Vec::with_capacity(INSERTERS * PER_INSERTER);
    for t in 0..INSERTERS {
        for j in 0..PER_INSERTER {
            let k = key(t as u64, (j / 64) as u32, (j as u64) * 256);
            let g = dram_gva((t * PER_INSERTER + j) as u64 * 8192 + 16);
            expected.push((k, g));
        }
    }
    let expected = Arc::new(expected);
    let stop = Arc::new(AtomicBool::new(false));
    let bad_hits = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(INSERTERS + LOOKERS));

    let mut lookers = Vec::new();
    for _ in 0..LOOKERS {
        let idx = idx.clone();
        let expected = expected.clone();
        let stop = stop.clone();
        let bad = bad_hits.clone();
        let barrier = barrier.clone();
        lookers.push(std::thread::spawn(move || {
            barrier.wait();
            let mut i = 0usize;
            while !stop.load(Ordering::Acquire) {
                let (k, g) = &expected[i % expected.len()];
                i += 1;
                // In-flight writes are PENDING → invisible: any Hit must be
                // the exact published GVA, never a stale or wrong one.
                if let Lookup::Hit(h) = idx.lookup(k) {
                    if h != *g {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    let mut inserters = Vec::new();
    for t in 0..INSERTERS {
        let idx = idx.clone();
        let expected = expected.clone();
        let barrier = barrier.clone();
        let base = t * PER_INSERTER;
        inserters.push(std::thread::spawn(move || {
            barrier.wait();
            for j in 0..PER_INSERTER {
                let (k, g) = expected[base + j];
                let slot = idx.insert_pending(&k).expect("capacity sized for stress load");
                // pre-publish: own entry must not be Hit (PENDING invisible)
                assert!(
                    !matches!(idx.lookup(&k), Lookup::Hit(_)),
                    "PENDING leaked as Hit before publish"
                );
                assert!(idx.publish(slot, g));
            }
        }));
    }
    for h in inserters {
        h.join().expect("inserter panicked");
    }
    stop.store(true, Ordering::Release);
    for h in lookers {
        h.join().expect("looker panicked");
    }
    assert_eq!(bad_hits.load(Ordering::Relaxed), 0, "no stale/wrong Hit during stress");
    // no lost publishes: every key resolves to its exact GVA
    for (k, g) in expected.iter() {
        match idx.lookup(k) {
            Lookup::Hit(h) => assert_eq!(h, *g, "wrong GVA for {k:?}"),
            other => panic!("lost publish for {k:?}: {other:?}"),
        }
    }
}

#[test]
fn t2_index_pending_invisible_until_publish_cross_thread() {
    let idx = Arc::new(Index::new(64));
    let inserted = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let published = Arc::new(AtomicBool::new(false));
    let k = key(42, 3, 99);
    let g = dram_gva(777);

    let w_idx = idx.clone();
    let w_ins = inserted.clone();
    let w_go = go.clone();
    let w_pub = published.clone();
    let writer = std::thread::spawn(move || {
        let slot = w_idx.insert_pending(&k).expect("insert");
        w_ins.store(true, Ordering::Release);
        while !w_go.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        assert!(w_idx.publish(slot, g));
        w_pub.store(true, Ordering::Release);
    });

    while !inserted.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    for _ in 0..20_000 {
        assert!(
            !matches!(idx.lookup(&k), Lookup::Hit(_)),
            "Hit observed before publish"
        );
    }
    go.store(true, Ordering::Release);
    writer.join().expect("writer panicked");
    assert!(published.load(Ordering::Acquire));
    match idx.lookup(&k) {
        Lookup::Hit(h) => assert_eq!(h, g),
        other => panic!("missing after publish: {other:?}"),
    }
}

// ==================================================================== T3

#[test]
fn t3_publish_after_write_ordering_and_racing_get_accounting() {
    let idx = Index::new(128);
    let k = key(3, 2, 512);
    let g = dram_gva(4096);
    let slot = idx.insert_pending(&k).expect("claim");
    // A get racing the in-flight put: Miss or Pending — the caller counts
    // Pending as pending_racing_gets; both are a miss by design (T3).
    let mut racing = 0u64;
    for _ in 0..1000 {
        match idx.lookup(&k) {
            Lookup::Miss => {}
            Lookup::Pending => racing += 1,
            Lookup::Hit(_) => panic!("lookup hit before publish"),
        }
    }
    assert!(racing <= 1000, "racing gets observable and countable by caller");
    // (the engine writes the payload to arena/layout here, THEN publishes)
    assert!(idx.publish(slot, g));
    assert!(matches!(idx.lookup(&k), Lookup::Hit(_)));
}

#[test]
fn t3_rollback_leaves_tombstone_and_slot_reusable() {
    let idx = Index::new(128);
    let k = key(5, 1, 128);
    let slot = idx.insert_pending(&k).expect("claim");
    assert!(idx.rollback(slot), "rollback of a PENDING slot");
    assert!(matches!(idx.lookup(&k), Lookup::Miss), "rolled-back write invisible");
    assert!(!idx.publish(slot, dram_gva(1)), "publish after rollback rejected");
    assert!(matches!(idx.lookup(&k), Lookup::Miss));
    // TOMBSTONE keeps probe chains intact AND is reusable: re-inserting the
    // same key claims the first acceptable slot in probe order — the tombstone.
    let slot2 = idx.insert_pending(&k).expect("re-claim of tombstone slot");
    assert_eq!(slot, slot2, "tombstone reclaimed by the next insert");
    assert!(idx.publish(slot2, dram_gva(2)));
    match idx.lookup(&k) {
        Lookup::Hit(g) => assert_eq!(g.offset(), 2),
        other => panic!("expected Hit after re-publish: {other:?}"),
    }
}

// ==================================================================== T4

#[test]
fn t4_row1_shared_refcount_wins_over_everything() {
    let cfg = Config::default();
    // refcount > 1 beats long context, long reply, and turn-1 (precedence).
    assert_eq!(
        decide(&sig(1, 2, 1, cfg.tau_tokens * 10, cfg.l_agent_tokens * 10), None, &cfg),
        Medium::Dram
    );
    assert_eq!(decide(&sig(1, 3, 1, 0, 0), None, &cfg), Medium::Dram);
    // refcount == 1 is unshared: later rows apply (here row 4, turn = 1).
    assert_eq!(decide(&sig(1, 1, 1, 0, 0), None, &cfg), Medium::Ssd);
}

#[test]
fn t4_row2_large_context_goes_ssd() {
    let cfg = Config::default();
    assert_eq!(
        decide(&sig(2, 1, 9, 0, cfg.l_agent_tokens), None, &cfg),
        Medium::Ssd,
        "ctx == L_agent qualifies (>=)"
    );
    assert_eq!(
        decide(&sig(2, 1, 9, 0, cfg.l_agent_tokens - 1), None, &cfg),
        Medium::Dram,
        "below L_agent falls through to later rows"
    );
}

#[test]
fn t4_row3_long_reply_goes_ssd() {
    let cfg = Config::default();
    assert_eq!(
        decide(&sig(3, 1, 9, cfg.tau_tokens, 0), None, &cfg),
        Medium::Ssd,
        "reply == tau qualifies (>=)"
    );
    assert_eq!(
        decide(&sig(3, 1, 9, cfg.tau_tokens - 1, 0), None, &cfg),
        Medium::Dram,
        "reply < tau falls through"
    );
}

#[test]
fn t4_row4_turn_one_oneshot_goes_ssd() {
    let cfg = Config::default();
    assert_eq!(decide(&sig(4, 1, 1, 0, 0), None, &cfg), Medium::Ssd);
    assert_eq!(
        decide(&sig(4, 1, 2, 0, 0), None, &cfg),
        Medium::Dram,
        "turn > 1 with short reply is row 5"
    );
}

#[test]
fn t4_row5_short_chat_turn_gt1_dram() {
    let cfg = Config::default();
    assert_eq!(decide(&sig(5, 1, 2, cfg.tau_tokens - 1, 0), None, &cfg), Medium::Dram);
    assert_eq!(decide(&sig(5, 1, 100, 0, 0), None, &cfg), Medium::Dram);
}

#[test]
fn t4_fallback_turn_zero_dram() {
    let cfg = Config::default();
    assert_eq!(decide(&sig(6, 1, 0, 0, 0), None, &cfg), Medium::Dram, "fallback row");
}

#[test]
fn t4_dram_full_spill_flips_scope_to_ssd_oq6() {
    let cfg = Config::default();
    let arena = Arena::with_capacity(4096);
    let reg = ScopeRegistry::new();
    let sess = 11;
    assert!(reg.create_scope(sess, Medium::Dram, 100));
    let mut spill_puts = 0u64;
    loop {
        match arena.reserve(1024) {
            Ok(off) => {
                arena.write(off, &[0x5Au8; 1024]);
                assert!(reg.add_dram_extent(sess, off, 1024));
            }
            Err(_) => {
                // DRAM-full spill (OQ-6): this put goes to SSD and the
                // scope flips to SSD; the engine counts metrics.dram_spill_puts.
                spill_puts += 1;
                assert!(reg.flip_to_ssd(sess));
                break;
            }
        }
    }
    assert_eq!(spill_puts, 1, "exactly one ArenaFull triggers the spill");
    assert_eq!(reg.sticky_medium(sess), Some(Medium::Ssd));
    // Once flipped, every subsequent put is SSD regardless of signals.
    assert_eq!(
        decide(&sig(sess, 1, 2, 0, 0), reg.sticky_medium(sess), &cfg),
        Medium::Ssd,
        "spilled scope: short-chat append stays SSD"
    );
    assert_eq!(
        decide(&sig(sess, 1, 1, cfg.tau_tokens, cfg.l_agent_tokens), reg.sticky_medium(sess), &cfg),
        Medium::Ssd
    );
}

// ==================================================================== T5

#[test]
fn t5_ssd_scope_appends_stay_ssd() {
    let cfg = Config::default();
    // Every Table-2 shape — including the ones that would be Dram fresh —
    // must stay SSD once the scope is SSD (on-disk contiguity invariant).
    let shapes = [
        sig(1, 1, 2, 0, 0),                                // short chat → Dram fresh
        sig(1, 2, 1, 0, 0),                                // shared → Dram fresh
        sig(1, 1, 1, cfg.tau_tokens, cfg.l_agent_tokens),  // SSD fresh anyway
        sig(1, 1, 0, 0, 0),                                // fallback → Dram fresh
    ];
    for s in &shapes {
        assert_eq!(decide(s, Some(Medium::Ssd), &cfg), Medium::Ssd);
    }
}

#[test]
fn t5_dram_scope_appends_reevaluate_and_can_flip() {
    let cfg = Config::default();
    let reg = ScopeRegistry::new();
    // DRAM scope: appends re-evaluate fresh — a long-context signal flips it.
    assert!(reg.create_scope(8, Medium::Dram, 50));
    let long_ctx = sig(8, 1, 5, 0, cfg.l_agent_tokens);
    assert_eq!(decide(&long_ctx, reg.sticky_medium(8), &cfg), Medium::Ssd);
    assert!(reg.flip_to_ssd(8));
    assert_eq!(reg.sticky_medium(8), Some(Medium::Ssd));
    // A short-chat append on a DRAM scope stays DRAM (row 5, no flip).
    assert!(reg.create_scope(9, Medium::Dram, 60));
    let short_chat = sig(9, 1, 3, 0, 0);
    assert_eq!(decide(&short_chat, reg.sticky_medium(9), &cfg), Medium::Dram);
    assert_eq!(reg.sticky_medium(9), Some(Medium::Dram));
}

// ==================================================================== T6

#[test]
fn t6_reclaim_never_frees_referenced_scope() {
    let cfg = Config::default();
    let index = Index::new(1024);
    let spy = SpyArena {
        inner: Arena::with_capacity(65536),
        index: &index,
        off2key: Mutex::new(HashMap::new()),
        freed_order: Mutex::new(Vec::new()),
    };
    let reg = ScopeRegistry::new();
    let keys = make_dram_scope(&spy, &index, &reg, 1, 100, 4);
    let used = spy.used_bytes();
    assert_eq!(used, 4096);
    let mut metrics = Metrics::default();
    // usage above the high watermark (0.9 × 65536 = 58982)
    let usage = 60_000u64;

    // reader-pinned scope must survive (D15: closes resolve→read TOCTOU)
    assert!(reg.pin_reader(1));
    let freed = reclaim_pass(Medium::Dram, usage, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 0);
    assert_eq!(metrics.reclaim_events, 0);
    assert!(reg.scope_exists(1));
    assert_eq!(spy.used_bytes(), used, "referenced scope extents untouched");
    for k in &keys {
        assert!(matches!(index.lookup(k), Lookup::Hit(_)));
    }
    reg.unpin_reader(1);

    // writer-pinned scope must survive (a PENDING write pins its scope)
    assert!(reg.pin_writer(1));
    let freed = reclaim_pass(Medium::Dram, usage, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 0);
    assert!(reg.scope_exists(1));
    reg.unpin_writer(1);

    // unreferenced now: reclaimed, keys tombstoned, extents freed
    let freed = reclaim_pass(Medium::Dram, usage, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 4096);
    assert_eq!(metrics.reclaim_events, 1);
    assert!(!reg.scope_exists(1));
    for k in &keys {
        assert!(matches!(index.lookup(k), Lookup::Miss), "reclaimed key must miss");
    }
    assert_eq!(spy.used_bytes(), 0);
    assert_eq!(metrics.reclaim_payload_ops, 0, "reclaim must stay metadata-only");
}

#[test]
fn t6_reclaim_tombstones_index_before_freeing_extents() {
    // The SpyArena asserts on every free_extent that the extent's key is no
    // longer Hit — if reclaim freed before invalidating, this test panics.
    let cfg = Config::default();
    let index = Index::new(1024);
    let spy = SpyArena {
        inner: Arena::with_capacity(65536),
        index: &index,
        off2key: Mutex::new(HashMap::new()),
        freed_order: Mutex::new(Vec::new()),
    };
    let reg = ScopeRegistry::new();
    let keys = make_dram_scope(&spy, &index, &reg, 1, 1, 8);
    let mut metrics = Metrics::default();
    let freed = reclaim_pass(Medium::Dram, 60_000, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 8192);
    assert_eq!(metrics.reclaim_events, 1);
    // every key already tombstoned before its extent was freed (spy held);
    // post-state: all keys miss, arena fully returned
    for k in &keys {
        assert!(matches!(index.lookup(k), Lookup::Miss));
    }
    assert_eq!(spy.used_bytes(), 0);
    let order = spy.freed_order.lock().unwrap().clone();
    assert_eq!(order.len(), 8);
}

#[test]
fn t6_reclaim_oldest_first_until_low_watermark() {
    let cfg = Config::default();
    let index = Index::new(1024);
    let spy = SpyArena {
        inner: Arena::with_capacity(100_000),
        index: &index,
        off2key: Mutex::new(HashMap::new()),
        freed_order: Mutex::new(Vec::new()),
    };
    let reg = ScopeRegistry::new();
    // three unreferenced scopes, creation order 1 < 2 < 3, 20 KiB each
    let _k1 = make_dram_scope(&spy, &index, &reg, 101, 1, 20);
    let _k2 = make_dram_scope(&spy, &index, &reg, 102, 2, 20);
    let k3 = make_dram_scope(&spy, &index, &reg, 103, 3, 20);
    assert_eq!(spy.used_bytes(), 61_440);
    let mut metrics = Metrics::default();

    // high limit = 90_000, low limit = 70_000 → must free scope 101, then 102
    let freed = reclaim_pass(Medium::Dram, 95_000, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 40_960, "frees oldest scopes until the low watermark");
    assert_eq!(metrics.reclaim_events, 2);
    assert!(!reg.scope_exists(101), "oldest scope reclaimed first");
    assert!(!reg.scope_exists(102));
    assert!(reg.scope_exists(103), "youngest scope survives");
    for k in &k3 {
        assert!(matches!(index.lookup(k), Lookup::Hit(_)), "survivor keys intact");
    }
    // strict oldest-first order in the free log: 101's 20 extents, then 102's
    let order = spy.freed_order.lock().unwrap().clone();
    let s1: Vec<u64> = (0..20u64).map(|i| i * 1024).collect();
    let s2: Vec<u64> = (0..20u64).map(|i| 20_480 + i * 1024).collect();
    let mut expect_order = s1;
    expect_order.extend(s2);
    assert_eq!(order, expect_order, "frees strictly oldest-scope-first");
    assert_eq!(metrics.reclaim_payload_ops, 0);
}

#[test]
fn t6_reclaim_advisory_when_nothing_is_reclaimable() {
    let cfg = Config::default();
    let index = Index::new(1024);
    let spy = SpyArena {
        inner: Arena::with_capacity(65536),
        index: &index,
        off2key: Mutex::new(HashMap::new()),
        freed_order: Mutex::new(Vec::new()),
    };
    let reg = ScopeRegistry::new();
    let keys = make_dram_scope(&spy, &index, &reg, 1, 1, 4);
    let mut metrics = Metrics::default();

    // pinned scope above the high watermark: advisory — nothing freed
    assert!(reg.pin_reader(1));
    let freed = reclaim_pass(Medium::Dram, 60_000, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 0);
    assert_eq!(metrics.reclaim_events, 0);
    for k in &keys {
        assert!(matches!(index.lookup(k), Lookup::Hit(_)));
    }
    reg.unpin_reader(1);

    // empty registry above the high watermark: advisory — nothing freed
    let reg2 = ScopeRegistry::new();
    let freed = reclaim_pass(Medium::Dram, 60_000, &cfg, &reg2, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 0);
    assert_eq!(metrics.reclaim_events, 0);
}

#[test]
fn t6_reclaim_ssd_path_uses_layout_and_packed_offsets() {
    let mut cfg = Config::default();
    cfg.gpu_file_bytes = 1 << 20;
    cfg.gpu_file_pool = 2;
    let index = Index::new(1024);
    let layout = FakeLayout {
        freed: Mutex::new(Vec::new()),
        file_cap: 1 << 20,
    };
    let reg = ScopeRegistry::new();

    // SSD GVA packing round-trip through the frozen resolve contract
    let g = ssd_gva(3, 5000);
    let fr = layout.resolve(g, 8192);
    assert_eq!(fr.file_seq, 3);
    assert_eq!(fr.in_file_off, 5000);
    assert_eq!(fr.len, 8192);

    // two unreferenced SSD scopes (oldest first), 300 KiB each
    assert!(reg.create_scope(201, Medium::Ssd, 1));
    assert!(reg.add_ssd_extent(201, 0, 4096, 300_000));
    let ka = key(201, 0, 0);
    publish_key(&index, &ka, ssd_gva(0, 4096));
    assert!(reg.add_key(201, ka));
    assert!(reg.create_scope(202, Medium::Ssd, 2));
    assert!(reg.add_ssd_extent(202, 1, 4096, 300_000));
    let kb = key(202, 0, 0);
    publish_key(&index, &kb, ssd_gva(1, 4096));
    assert!(reg.add_key(202, kb));

    let mut metrics = Metrics::default();
    // capacity = gpu_file_pool × gpu_file_bytes = 2 MiB (§5 watermark denominator)
    let freed = reclaim_pass(Medium::Ssd, 1_900_000, &cfg, &reg, &index, ReclaimStore::Ssd(&layout), &mut metrics);
    assert_eq!(freed, 600_000, "drains from above 0.9 to below 0.7 of 2 MiB");
    assert_eq!(metrics.reclaim_events, 2);
    assert_eq!(
        *layout.freed.lock().unwrap(),
        vec![(0u32, 4096u64, 300_000u64), (1, 4096, 300_000)],
        "layout.free_extent called with (file_seq, in-file off, len), oldest first"
    );
    assert!(!reg.scope_exists(201));
    assert!(!reg.scope_exists(202));
    assert!(matches!(index.lookup(&ka), Lookup::Miss));
    assert!(matches!(index.lookup(&kb), Lookup::Miss));
    assert_eq!(metrics.reclaim_payload_ops, 0);
}

#[test]
fn t6_reclaim_frees_dram_extents_of_flipped_scope() {
    // OQ-6: a scope that flipped to SSD mid-life still owns DRAM
    // extents; the DRAM pass must free exactly those without touching the
    // scope's SSD keys/extents; the SSD pass then retires the rest.
    let mut cfg = Config::default();
    cfg.gpu_file_bytes = 1 << 20;
    cfg.gpu_file_pool = 2;
    let index = Index::new(1024);
    let spy = SpyArena {
        inner: Arena::with_capacity(65536),
        index: &index,
        off2key: Mutex::new(HashMap::new()),
        freed_order: Mutex::new(Vec::new()),
    };
    let reg = ScopeRegistry::new();

    assert!(reg.create_scope(301, Medium::Dram, 5));
    let kd = key(301, 0, 0);
    let offd = spy.reserve(1024).expect("dram extent");
    publish_key(&index, &kd, dram_gva(offd));
    spy.register(offd, kd);
    assert!(reg.add_dram_extent(301, offd, 1024));
    assert!(reg.add_key(301, kd));
    // spill flip, then an SSD append
    assert!(reg.flip_to_ssd(301));
    let ks = key(301, 0, 256);
    assert!(reg.add_ssd_extent(301, 0, 8192, 1024));
    publish_key(&index, &ks, ssd_gva(0, 8192));
    assert!(reg.add_key(301, ks));

    let layout = FakeLayout {
        freed: Mutex::new(Vec::new()),
        file_cap: 1 << 20,
    };
    let mut metrics = Metrics::default();

    // DRAM pass: frees the dram extent only; scope stays (SSD parts intact)
    let freed = reclaim_pass(Medium::Dram, 60_000, &cfg, &reg, &index, ReclaimStore::Dram(&spy), &mut metrics);
    assert_eq!(freed, 1024);
    assert!(reg.scope_exists(301), "scope kept while SSD extents remain");
    assert!(matches!(index.lookup(&kd), Lookup::Miss), "dram key tombstoned");
    assert!(matches!(index.lookup(&ks), Lookup::Hit(_)), "ssd key untouched by dram pass");
    assert_eq!(metrics.reclaim_events, 0, "no removal until the scope is fully retired");

    // SSD pass completes the scope
    let freed = reclaim_pass(Medium::Ssd, 1_900_000, &cfg, &reg, &index, ReclaimStore::Ssd(&layout), &mut metrics);
    assert_eq!(freed, 1024);
    assert!(!reg.scope_exists(301));
    assert!(matches!(index.lookup(&ks), Lookup::Miss));
    assert_eq!(metrics.reclaim_events, 1, "one removal across both passes");
    assert_eq!(metrics.reclaim_payload_ops, 0);
}

#[test]
fn t6_registry_pin_semantics_and_unknown_sessions() {
    let reg = ScopeRegistry::new();
    assert!(!reg.pin_reader(1), "unknown session");
    assert!(!reg.pin_writer(1), "unknown session");
    assert_eq!(reg.sticky_medium(1), None);
    assert!(!reg.add_dram_extent(1, 0, 8), "no scope to attach to");
    assert!(!reg.add_key(1, key(1, 0, 0)));

    assert!(reg.create_scope(1, Medium::Dram, 10), "first create");
    assert!(!reg.create_scope(1, Medium::Ssd, 20), "idempotent: first creation wins");
    assert_eq!(reg.sticky_medium(1), Some(Medium::Dram), "oldest-first ts preserved");

    assert!(reg.pin_reader(1));
    assert!(reg.pin_reader(1));
    assert!(reg.pin_writer(1));
    // extra unpins are ignored (saturating, no underflow)
    reg.unpin_reader(1);
    reg.unpin_reader(1);
    reg.unpin_reader(1);
    reg.unpin_writer(1);
    reg.unpin_writer(1);

    assert!(reg.drop_scope(1));
    assert!(!reg.drop_scope(1), "double drop");
    assert!(!reg.pin_reader(1));
    assert!(!reg.scope_exists(1));
}

// ================================================================== arena

#[test]
fn arena_reserve_write_read_roundtrip() {
    let a = Arena::with_capacity(8192);
    let off = a.reserve(1000).expect("empty arena");
    assert_eq!(off, 0);
    let data: Vec<u8> = (0..1000u64).map(|i| (i % 251) as u8).collect();
    a.write(off, &data);
    let mut out = vec![0u8; 1000];
    a.read(off, 1000, &mut out);
    assert_eq!(out, data);
    assert_eq!(a.capacity(), 8192);
    assert_eq!(a.used_bytes(), 1000);
    // trait-object use (engine/reclaim consume &dyn DramArena)
    let d: &dyn DramArena = &a;
    let off2 = d.reserve(100).expect("bump");
    assert_eq!(off2, 1000);
}

#[test]
fn arena_first_fit_reuses_freed_extents_in_offset_order() {
    let a = Arena::with_capacity(8192);
    let a0 = a.reserve(100).expect("a");
    let b0 = a.reserve(100).expect("b");
    let c0 = a.reserve(100).expect("c");
    assert_eq!((a0, b0, c0), (0, 100, 200));
    a.free_extent(a0, 100);
    a.free_extent(c0, 100);
    // first fit picks the LOWEST-offset hole that fits, splitting the remainder
    assert_eq!(a.reserve(50).expect("reuse a's hole"), 0);
    assert_eq!(a.reserve(50).expect("hole split remainder"), 50);
    assert_eq!(a.reserve(100).expect("c's hole"), 200);
    assert_eq!(a.used_bytes(), 300);
}

#[test]
fn arena_reserve_fails_when_full_then_recovers_after_free() {
    let a = Arena::with_capacity(1024);
    assert!(a.reserve(600).is_ok());
    assert!(matches!(a.reserve(600), Err(ArenaFull)), "neither bump nor free list can satisfy");
    a.free_extent(0, 600);
    assert_eq!(a.reserve(600).expect("freed extent reused"), 0);
    assert!(matches!(a.reserve(600), Err(ArenaFull)));
}

#[test]
fn arena_adjacent_frees_coalesce() {
    let a = Arena::with_capacity(700);
    let offs: Vec<u64> = (0..4).map(|_| a.reserve(100).expect("alloc")).collect();
    assert_eq!(offs, vec![0, 100, 200, 300]);
    a.free_extent(100, 100);
    a.free_extent(200, 100); // adjacent → one (100,200) extent
    a.free_extent(0, 100); // adjacent → one (0,300) extent
    assert_eq!(
        a.reserve(300).expect("coalesced hole"),
        0,
        "a request no single pre-coalesce hole could satisfy"
    );
    assert_eq!(a.reserve(100).expect("bump continues"), 400);
    assert!(matches!(a.reserve(300), Err(ArenaFull)));
    assert_eq!(a.used_bytes(), 500, "0..300 reused hole + 300..400 + 400..500 live");
}

#[test]
fn arena_used_bytes_tracks_live_data() {
    let a = Arena::with_capacity(1000);
    assert_eq!(a.used_bytes(), 0);
    let x = a.reserve(400).expect("alloc");
    assert_eq!(a.used_bytes(), 400);
    a.free_extent(x, 100);
    assert_eq!(a.used_bytes(), 300, "used = bump_high - freed_bytes");
    assert_eq!(a.reserve(100).expect("hole reused before bump grows"), 0);
    assert_eq!(a.used_bytes(), 400);
}

#[test]
fn arena_concurrent_reserve_write_verify_free_stress() {
    let a = Arc::new(Arena::with_capacity(1 << 20));
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let a = a.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for i in 0..200u64 {
                let len = 1024 + (i % 4) * 256;
                let off = a.reserve(len).expect("pool sized for 8 live extents");
                let mut buf = vec![0u8; len as usize];
                for (idx, chunk) in buf.chunks_exact_mut(8).enumerate() {
                    let word = t ^ (i << 8) ^ (idx as u64);
                    chunk.copy_from_slice(&word.to_le_bytes());
                }
                a.write(off, &buf);
                let mut out = vec![0u8; len as usize];
                a.read(off, len, &mut out);
                assert_eq!(out, buf, "arena bytes corrupted under concurrency");
                a.free_extent(off, len);
            }
        }));
    }
    for h in handles {
        h.join().expect("arena stress thread panicked");
    }
    assert_eq!(a.used_bytes(), 0, "all extents freed and accounted");
}

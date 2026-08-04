package jp.houlab.mochidsuki.moymoy;

import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

import net.minecraft.nbt.CompoundTag;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.saveddata.SavedData;

/**
 * Persistent per-op idempotency for emerald charges AND withdrawals (DEV.md
 * consistency model).
 *
 * <p>Charges map {@code op_id → consumed エメ}. A charge command is consumed at
 * most once: a replay of a settled {@code op_id} re-acks the SAME consumed
 * amount without touching the inventory again, so an at-least-once command bus
 * + a lost ack can never double-consume. Persisted as world {@link SavedData}
 * so the claim survives a server restart (the "consume after restart"
 * double-spend guard).
 *
 * <p>Withdrawals map {@code op_id → grant state} in a SEPARATE {@code grants}
 * compound (charges keep using {@code ops}, untouched, for backward
 * compatibility with existing worlds). A withdrawal mints emeralds from
 * nothing, so replaying a lost ack must never re-mint — see {@link
 * MoyMoyExtension}'s class Javadoc for the two-phase claim/confirm design this
 * backs, and {@link #grantState}/{@link #claimGrant}/{@link #recordGrant} for
 * the state machine ({@code null} → {@code -1} claimed → {@code ≥0} confirmed).
 *
 * <p>Stored on the overworld's data storage under {@code moymoy_emerald_ops}.
 *
 * <p><b>Known pre-existing residual window (shared by {@code ops} and {@code
 * grants}, not introduced or fixed here):</b> {@link #load} is only reached
 * via {@code DimensionDataStorage#get}, which catches any exception while
 * reading/parsing the {@code .dat} file, logs it, and treats the store as
 * absent — i.e. a corrupted or unreadable save file silently resets ALL
 * recorded ops (both charges and withdrawal grants) to empty. A replay after
 * such corruption would re-consume/re-grant. This is a limitation of the
 * vanilla {@code SavedData} loading path, not of this class.
 */
public final class EmeraldOpStore extends SavedData {

    private static final String DATA_NAME = "moymoy_emerald_ops";

    /** op_id → consumed エメ. Concurrent: written from the connector IO thread. */
    private final Map<String, Integer> consumed = new ConcurrentHashMap<>();

    /**
     * op_id → grant state: {@code -1} = claimed, not yet confirmed; {@code ≥0} =
     * confirmed granted エメ. Always mutated on the server thread (see {@link
     * MoyMoyExtension#handle}'s single {@code server.submit} block for
     * withdrawals) — {@code ConcurrentHashMap} is used anyway for consistency
     * with {@link #consumed} and to tolerate a future concurrent reader.
     */
    private final Map<String, Integer> grants = new ConcurrentHashMap<>();

    public EmeraldOpStore() {}

    /** Load/create the store from the overworld's data storage. (1.20.1 signature:
     *  {@code computeIfAbsent(Function<CompoundTag,T> load, Supplier<T> ctor, String)}.) */
    public static EmeraldOpStore get(MinecraftServer server) {
        ServerLevel level = server.overworld();
        return level.getDataStorage().computeIfAbsent(
                EmeraldOpStore::load, EmeraldOpStore::new, DATA_NAME);
    }

    public static EmeraldOpStore load(CompoundTag tag) {
        EmeraldOpStore store = new EmeraldOpStore();
        CompoundTag ops = tag.getCompound("ops");
        for (String key : ops.getAllKeys()) {
            store.consumed.put(key, ops.getInt(key));
        }
        // getCompound() on a missing key returns an empty CompoundTag rather than
        // null, so worlds saved before emerald.withdraw existed (no "grants" key
        // at all) load here as an empty — not absent — grants map. No migration
        // needed.
        CompoundTag grantsTag = tag.getCompound("grants");
        for (String key : grantsTag.getAllKeys()) {
            store.grants.put(key, grantsTag.getInt(key));
        }
        return store;
    }

    @Override
    public CompoundTag save(CompoundTag tag) {
        CompoundTag ops = new CompoundTag();
        for (Map.Entry<String, Integer> e : new HashMap<>(consumed).entrySet()) {
            ops.putInt(e.getKey(), e.getValue());
        }
        tag.put("ops", ops);

        CompoundTag grantsTag = new CompoundTag();
        for (Map.Entry<String, Integer> e : new HashMap<>(grants).entrySet()) {
            grantsTag.putInt(e.getKey(), e.getValue());
        }
        tag.put("grants", grantsTag);
        return tag;
    }

    /** The consumed amount previously recorded for {@code opId}, or {@code null}. */
    public Integer recorded(String opId) {
        return consumed.get(opId);
    }

    /** Claim {@code opId} as consumed, persisting immediately ({@link #setDirty()}). */
    public void record(String opId, int amount) {
        consumed.put(opId, amount);
        setDirty();
    }

    // ── withdrawal (grant) state ─────────────────────────────────────────────

    /**
     * The recorded grant state for a withdrawal {@code op_id}: {@code null} if
     * never claimed, {@code -1} if claimed but not yet confirmed (the server
     * crashed between {@link #claimGrant} and {@link #recordGrant} — see the
     * class Javadoc), or the confirmed granted エメ ({@code ≥0}) otherwise.
     */
    public Integer grantState(String opId) {
        return grants.get(opId);
    }

    /**
     * Claims {@code opId} for a withdrawal by atomically recording it as
     * claimed-but-unconfirmed ({@code -1}) if and only if it had no prior state,
     * returning whether this call performed the claim. Callers MUST NOT grant
     * emeralds when this returns {@code false} — {@code opId} was already
     * claimed or confirmed by (in practice) an earlier call on the server
     * thread.
     *
     * <p>Does not flush to disk — see {@link #flush}, which the caller MUST
     * invoke before granting anything (that ordering is the entire point of the
     * two-phase design; see {@link MoyMoyExtension}'s class Javadoc).
     */
    public boolean claimGrant(String opId) {
        boolean claimed = grants.putIfAbsent(opId, -1) == null;
        if (claimed) {
            setDirty();
        }
        return claimed;
    }

    /**
     * Records the confirmed granted エメ for a claimed {@code opId}. Does not
     * flush to disk — see {@link #flush}.
     */
    public void recordGrant(String opId, int amount) {
        grants.put(opId, amount);
        setDirty();
    }

    /**
     * Synchronously flushes every {@link SavedData} on the overworld — including
     * this store — to disk, on the calling thread. MUST be called on the server
     * thread.
     *
     * <p>{@link #setDirty()} only flags dirty; MC's own autosave is what
     * normally clears it, on its own schedule — far too late for a
     * withdrawal's claim/confirm records, which must already be on disk before
     * the next line of code touches a player's inventory (a crash between claim
     * and confirm MUST be observable as {@code -1} on restart, not silently
     * lost). {@link net.minecraft.world.level.storage.DimensionDataStorage#save()}
     * exists on 1.20.1 (Forge 47.4.x) — confirmed by compiling against it, since
     * it is not part of any documented/stable modding API — and does exactly
     * this: it walks every cached {@link SavedData} and calls its dirty-gated
     * {@code save(File)}.
     *
     * <p><b>Residual crash window this can't close (a limitation of the vanilla
     * API being reused, not of this store):</b> {@code SavedData#save(File)}
     * catches and only LOGS a failed disk write (full disk, permissions, ...),
     * then unconditionally clears the dirty flag anyway — so on a write failure
     * this method returns normally with the claim NOT actually on disk, and
     * nothing in this API (including {@code isDirty()} afterwards) can detect
     * that. A server crash following such a failure loses the claim, and a
     * replay would then re-grant.
     *
     * <p>Also flushes every OTHER dirty {@link SavedData} on the overworld
     * (raid data, map data, ...) — that's the granularity the vanilla API
     * offers; there is no narrower "flush just this one store" call.
     */
    public static void flush(MinecraftServer server) {
        server.overworld().getDataStorage().save();
    }
}

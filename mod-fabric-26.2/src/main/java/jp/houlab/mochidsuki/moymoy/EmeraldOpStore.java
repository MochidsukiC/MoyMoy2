package jp.houlab.mochidsuki.moymoy;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

import com.mojang.serialization.Codec;
import com.mojang.serialization.codecs.RecordCodecBuilder;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;
import net.minecraft.resources.Identifier;
import net.minecraft.server.MinecraftServer;
import net.minecraft.util.datafix.DataFixTypes;
import net.minecraft.world.level.saveddata.SavedData;
import net.minecraft.world.level.saveddata.SavedDataType;
import net.minecraft.world.level.storage.LevelResource;

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
 * <p>Stored on the SERVER-wide data storage ({@link
 * MinecraftServer#getDataStorage()}) under the id {@code moymoy:emerald_ops},
 * i.e. the file {@code <world>/data/moymoy/emerald_ops.dat}. See {@link #get}
 * for why this is NOT the overworld's per-dimension storage on 26.2.
 *
 * <p><b>Known pre-existing residual window (shared by {@code ops} and {@code
 * grants}, not introduced or fixed here):</b> {@link #CODEC} is only reached
 * via {@code SavedDataStorage#readSavedData}, whose whole body is wrapped in a
 * {@code catch (Exception)} that logs {@code "Error loading saved data: {}"}
 * and returns {@code null} — i.e. a corrupted or unreadable {@code .dat} file
 * silently resets ALL recorded ops (both charges and withdrawal grants) to
 * empty. A replay after such corruption would re-consume/re-grant. This is a
 * limitation of the vanilla {@code SavedData} loading path, not of this class,
 * and 26.2 kept it structurally identical to 1.20.1 (only the class holding it
 * was renamed {@code DimensionDataStorage} → {@code SavedDataStorage}).
 */
public final class EmeraldOpStore extends SavedData {

    private static final Logger LOGGER = LoggerFactory.getLogger("moymoy");

    /**
     * The 1.20.1 file this store used to live in, relative to {@code
     * <world>/data}. On 26.2 the same ledger lands in {@code
     * <world>/data/moymoy/emerald_ops.dat} instead (see {@link #TYPE}), so the
     * old file is a different path and must be migrated explicitly — see
     * {@link #migrateLegacyStore}.
     */
    private static final String LEGACY_FILE_NAME = "moymoy_emerald_ops.dat";

    /**
     * Codec for the on-disk form: {@code {ops:{op_id:int,...},
     * grants:{op_id:int,...}}}.
     *
     * <p>Both fields use {@code optionalFieldOf(name, Map.of())} rather than
     * {@code fieldOf(name)} ON PURPOSE. On 1.20.1 the hand-written loader read
     * them with {@code CompoundTag#getCompound}, which returns an EMPTY
     * compound — not null — for a missing key, so a world saved before {@code
     * emerald.withdraw} existed (no {@code "grants"} key at all) loaded as an
     * empty grants map. A plain {@code fieldOf} would instead fail the decode
     * for those worlds, and a failed decode is swallowed into "no saved data"
     * by {@code SavedDataStorage#readSavedData} — i.e. it would silently wipe
     * the {@code ops} ledger too. {@code optionalFieldOf} reproduces the 1.20.1
     * behaviour exactly.
     *
     * <p>Side effect of the default value, harmless: on ENCODE a field equal to
     * its default is omitted, so an empty ledger writes {@code {}} instead of
     * {@code {ops:{},grants:{}}}. It decodes back to the same empty maps.
     */
    public static final Codec<EmeraldOpStore> CODEC = RecordCodecBuilder.create(i -> i.group(
            Codec.unboundedMap(Codec.STRING, Codec.INT).optionalFieldOf("ops", Map.of())
                    .forGetter((EmeraldOpStore s) -> Map.copyOf(s.consumed)),
            Codec.unboundedMap(Codec.STRING, Codec.INT).optionalFieldOf("grants", Map.of())
                    .forGetter((EmeraldOpStore s) -> Map.copyOf(s.grants))
    ).apply(i, EmeraldOpStore::new));

    /**
     * The 26.2 {@code SavedData} descriptor: id, no-arg constructor, codec and
     * a {@link DataFixTypes}.
     *
     * <p>The id {@code moymoy:emerald_ops} becomes {@code
     * <world>/data/moymoy/emerald_ops.dat} — {@code
     * SavedDataStorage#getDataFile} does {@code
     * id.withSuffix(".dat").resolveAgainst(dataFolder)} and {@code
     * Identifier#resolveAgainst} resolves {@code namespace} then {@code path}
     * against the root.
     *
     * <p><b>Why {@code SAVED_DATA_WEATHER}:</b> {@link DataFixTypes} is a
     * closed 31-constant enum with no neutral/MOD value, so a mod MUST borrow
     * one. {@code SAVED_DATA_WEATHER} is the least harmful choice, verified
     * against the 26.2 jar with {@code javap}:
     * <ul>
     *   <li>{@code V99#registerTypes} registers {@code
     *       References.SAVED_DATA_WEATHER} with the supplier {@code
     *       DSL::remainder} (bootstrap method resolves to {@code
     *       com/mojang/datafixers/DSL.remainder()}), i.e. a pure pass-through
     *       type that touches no field. {@code V1460} re-registers it the same
     *       way.</li>
     *   <li>No {@code net.minecraft.util.datafix.fixes.*Fix} class references
     *       {@code SAVED_DATA_WEATHER} at all — the only classes in the whole
     *       {@code util/datafix} package that mention it are {@code
     *       DataFixTypes}, {@code References}, {@code V99} and {@code
     *       V1460}.</li>
     * </ul>
     * So running our tag through this type's fixer chain is a no-op for every
     * data version, which is exactly what a mod-owned tag needs.
     */
    public static final SavedDataType<EmeraldOpStore> TYPE = new SavedDataType<>(
            Identifier.fromNamespaceAndPath("moymoy", "emerald_ops"),
            EmeraldOpStore::new,
            CODEC,
            DataFixTypes.SAVED_DATA_WEATHER);

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

    /** Fresh, empty store. Used as {@link SavedDataType}'s constructor supplier. */
    public EmeraldOpStore() {}

    /**
     * Decode constructor. {@link #CODEC} hands back IMMUTABLE maps, so their
     * contents are copied into the live {@link ConcurrentHashMap}s rather than
     * kept by reference.
     */
    private EmeraldOpStore(Map<String, Integer> ops, Map<String, Integer> grants) {
        this.consumed.putAll(ops);
        this.grants.putAll(grants);
    }

    /**
     * Load/create the store from the SERVER-wide data storage.
     *
     * <p><b>Deliberately {@link MinecraftServer#getDataStorage()}, NOT {@code
     * server.overworld().getDataStorage()}.</b> On 1.20.1 the overworld's data
     * storage was rooted at {@code <world>/data} because {@code
     * DimensionType#getStorageFolder} special-cased the overworld. On 26.2 that
     * special case is gone — {@code getStorageFolder} unconditionally returns
     * {@code <root>/dimensions/<namespace>/<path>} — so the overworld's storage
     * is now {@code <world>/dimensions/minecraft/overworld/data}, a DIFFERENT
     * directory from the one 1.20.1 wrote into. {@code
     * MinecraftServer#getDataStorage()} (new in 26.2) is rooted at {@code
     * LevelResource.DATA} = {@code <world>/data}, which both matches the old
     * location and is semantically right: an op ledger is per-WORLD, not
     * per-dimension.
     */
    public static EmeraldOpStore get(MinecraftServer server) {
        return server.getDataStorage().computeIfAbsent(TYPE);
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
     * Flushes every {@link SavedData} on the server-wide storage — including
     * this store — to disk, blocking the calling thread until the write
     * completes. MUST be called on the server thread.
     *
     * <p>{@link #setDirty()} only flags dirty; MC's own autosave is what
     * normally clears it, on its own schedule — far too late for a
     * withdrawal's claim/confirm records, which must already be on disk before
     * the next line of code touches a player's inventory (a crash between claim
     * and confirm MUST be observable as {@code -1} on restart, not silently
     * lost). {@code SavedDataStorage#saveAndJoin()} is 26.2's replacement for
     * 1.20.1's {@code DimensionDataStorage#save()} and does the same job: it
     * encodes every dirty {@link SavedData} and writes it out.
     *
     * <p><b>26.2 differences worth knowing (all verified against the 26.2 jar
     * with {@code javap}):</b>
     * <ul>
     *   <li>{@code saveAndJoin()} is literally {@code scheduleSave().join()}.
     *       The actual file writes happen asynchronously on {@code
     *       Util.ioPool()}; the calling thread only blocks in {@code join}.
     *       Vanilla {@code MinecraftServer} saves the same way, so this is the
     *       supported shape, not a misuse.</li>
     *   <li>{@code scheduleSave()} chains onto {@code pendingWriteFuture} with
     *       {@code thenCompose}, so it also waits for an autosave that is
     *       already in flight. The server thread can therefore be parked longer
     *       here than 1.20.1's straight-line synchronous write ever was.</li>
     *   <li>{@code scheduleSave()} throws {@link IllegalStateException}
     *       ({@code "Trying to schedule save when SavedDataStorage is already
     *       closed"}) once the storage has been closed. That failure mode did
     *       not exist on 1.20.1; a withdrawal that arrives late during server
     *       shutdown lands on it.</li>
     * </ul>
     *
     * <p><b>Residual crash window this can't close (a limitation of the vanilla
     * API being reused, not of this store):</b> {@code
     * SavedDataStorage#collectDirtyTagsToSave} clears the dirty flag ({@code
     * setDirty(false)}) as soon as it has ENCODED a store, before anything is
     * written, and {@code tryWrite} catches a failed disk write (full disk,
     * permissions, ...) and only LOGS {@code "Could not save data to {}"} — so
     * on a write failure this method returns normally with the claim NOT
     * actually on disk, and nothing in this API (including {@code isDirty()}
     * afterwards) can detect that. A server crash following such a failure
     * loses the claim, and a replay would then re-grant. 26.2 reproduced
     * 1.20.1's defect structure exactly here.
     *
     * <p>Also flushes every OTHER dirty {@link SavedData} on the server-wide
     * storage (scoreboard, command storage, ...) — that's the granularity the
     * vanilla API offers; there is no narrower "flush just this one store"
     * call.
     */
    public static void flush(MinecraftServer server) {
        server.getDataStorage().saveAndJoin();
    }

    // ── 1.20.1 → 26.2 file migration ─────────────────────────────────────────

    /**
     * Moves a 1.20.1-era ledger at {@code <world>/data/moymoy_emerald_ops.dat}
     * into this store's 26.2 location, once, at {@code SERVER_STARTING}.
     *
     * <p><b>Why this is mandatory, not a nicety:</b> 26.2 addresses saved data
     * by {@link Identifier}, so our file moved from {@code
     * <world>/data/moymoy_emerald_ops.dat} to {@code
     * <world>/data/moymoy/emerald_ops.dat}. Vanilla will not do this for us —
     * {@code DimensionStorageFileFix} only relocates vanilla-named {@code .dat}
     * files, and {@code RemoveObsoleteFilesFileFix} does not cover ours either
     * — so the old file simply stays behind as an orphan and the new store is
     * created EMPTY. With an empty ledger, a replayed {@code emerald.charge}
     * whose {@code op_id} was already settled would consume a second time, and
     * a replayed {@code emerald.withdraw} would mint a second time. That is the
     * exact double-spend / double-mint this class exists to prevent, so the
     * upgrade must not proceed without carrying the ledger across.
     *
     * <p>Idempotent: entries are merged with put-if-absent semantics and the
     * old file is only renamed to {@code .migrated} AFTER a successful flush,
     * so re-running it (including after a failed rename) changes nothing.
     *
     * @throws IllegalStateException if the legacy file exists but cannot be
     *         read or does not have the expected shape. Starting {@code serve()}
     *         with an unreadable ledger is more dangerous than failing to boot,
     *         so this is deliberately NOT swallowed. A merely ABSENT legacy file
     *         is the normal path, not an error.
     */
    public static void migrateLegacyStore(MinecraftServer server) {
        Path legacy = server.getWorldPath(LevelResource.DATA).resolve(LEGACY_FILE_NAME);
        if (!Files.exists(legacy)) {
            // Normal operation on every boot after the first (and on new worlds).
            return;
        }

        CompoundTag root;
        try {
            // Read the raw NBT, no DataFixer: the root is
            // {data:{ops:{...},grants:{...}}, DataVersion:n} written by 1.20.1's
            // SavedData#save(File), and TYPE's DataFixTypes.SAVED_DATA_WEATHER is
            // registered as DSL.remainder() — a pass-through — so running the
            // fixer chain over it could not change a single byte anyway.
            root = NbtIo.readCompressed(legacy, NbtAccounter.unlimitedHeap());
        } catch (IOException | RuntimeException e) {
            LOGGER.error("Failed to read the legacy emerald op ledger at {}", legacy, e);
            throw new IllegalStateException(
                    "Failed to read the legacy emerald op ledger: " + legacy, e);
        }

        // 1.20.1's SavedData#save(File) ALWAYS wrote the "data" wrapper, so its
        // absence means this file is not the ledger we think it is. Bail out loudly
        // rather than treat a file we cannot parse as an empty ledger and rename it
        // away — that would lose the very records this migration exists to keep.
        CompoundTag data = root.getCompound("data").orElseThrow(() -> {
            LOGGER.error("Legacy emerald op ledger at {} has no \"data\" compound", legacy);
            return new IllegalStateException(
                    "Legacy emerald op ledger has no \"data\" compound: " + legacy);
        });

        // getCompoundOrEmpty mirrors 1.20.1's getCompound-returns-empty behaviour:
        // a world saved before emerald.withdraw existed simply has no "grants" key.
        CompoundTag legacyOps = data.getCompoundOrEmpty("ops");
        CompoundTag legacyGrants = data.getCompoundOrEmpty("grants");

        EmeraldOpStore store = get(server);

        // putIfAbsent, never put: the new store WINS on any op_id it already knows.
        // The state machine is monotonic (null → -1 claimed → ≥0 confirmed) and the
        // old file is by definition the older snapshot, so overwriting could push a
        // confirmed grant (≥0) back to claimed (-1) — after which the two-phase
        // logic in MoyMoyExtension would treat the withdrawal as never completed
        // and mint the emeralds a second time. The same applies to a settled charge
        // being replaced by a stale consumed amount.
        // getIntOr(key, 0) reproduces 1.20.1's getInt(key), which likewise yielded 0
        // for a missing or wrongly-typed value.
        int movedOps = 0;
        for (String opId : legacyOps.keySet()) {
            if (store.consumed.putIfAbsent(opId, legacyOps.getIntOr(opId, 0)) == null) {
                movedOps++;
            }
        }
        int movedGrants = 0;
        for (String opId : legacyGrants.keySet()) {
            if (store.grants.putIfAbsent(opId, legacyGrants.getIntOr(opId, 0)) == null) {
                movedGrants++;
            }
        }

        if (movedOps > 0 || movedGrants > 0) {
            store.setDirty();
        }
        // Flush unconditionally, even when nothing moved: the rename below is what
        // makes this migration non-repeatable, so the merged ledger must be on disk
        // before the source is taken away. When nothing is dirty this costs
        // nothing — scheduleSave() returns an already-completed future.
        flush(server);

        LOGGER.info("Migrated legacy emerald op ledger {}: {} ops, {} grants",
                LEGACY_FILE_NAME, movedOps, movedGrants);

        // Only now, with the merge durable, retire the old file. A failure here is
        // not fatal: the next boot reads the same file again and the put-if-absent
        // merge above is a no-op, so we log and carry on rather than refusing to
        // start. (A leftover .migrated file from an earlier partial run makes
        // Files.move throw FileAlreadyExistsException, which lands here too.)
        Path retired = legacy.resolveSibling(LEGACY_FILE_NAME + ".migrated");
        try {
            Files.move(legacy, retired);
        } catch (IOException e) {
            LOGGER.warn("Migrated the legacy emerald op ledger but could not rename {} to {}"
                    + "; it will be re-read (harmlessly) on the next start", legacy, retired, e);
        }
    }
}

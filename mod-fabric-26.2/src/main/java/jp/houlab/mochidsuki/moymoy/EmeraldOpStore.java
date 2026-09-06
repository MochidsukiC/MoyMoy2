package jp.houlab.mochidsuki.moymoy;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.file.Files;
import java.nio.file.NoSuchFileException;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.nio.file.attribute.BasicFileAttributes;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.ConcurrentHashMap;

import com.mojang.serialization.Codec;
import com.mojang.serialization.codecs.RecordCodecBuilder;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.IntTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;
import net.minecraft.nbt.Tag;
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

    /** Suffix the legacy file gets once its records are safely in the 26.2 store. */
    private static final String MIGRATED_SUFFIX = ".migrated";

    /**
     * Suffix for the copy of the 26.2 ledger taken before the migration writes to
     * it, so a failed write can be undone — see {@link #migrateLegacyStore}.
     */
    private static final String BACKUP_SUFFIX = ".premigration";

    /** What the migration calls the 1.20.1 file in logs and exception messages. */
    private static final String LEGACY_LEDGER = "Legacy emerald op ledger";

    /** What it calls the 26.2 file it wrote, when reading it back to verify. */
    private static final String MIGRATED_LEDGER = "Migrated emerald op ledger";

    /**
     * Smallest legal {@code ops} value. {@link MoyMoyExtension}'s charge path only
     * calls {@link #record} when it really consumed something ({@code consumed >
     * 0}), so a zero or negative charge record cannot have been written by us.
     */
    private static final int MIN_OPS_VALUE = 1;

    /**
     * Smallest legal {@code grants} value: {@code -1} is "claimed, not yet
     * confirmed" and anything {@code ≥ 0} is a confirmed grant.
     */
    private static final int MIN_GRANTS_VALUE = -1;

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
     *
     * <p><b>Do NOT "harden" this to {@code fieldOf("ops")}.</b> It looks like the
     * safer choice — a ledger with no {@code ops} key is certainly suspicious —
     * but the omit-on-default encode above makes it a double-mint bug. A store
     * that has grants but no charges (every charge settled and pruned, or a
     * world that has only ever withdrawn) encodes as {@code {grants:{...}}} with
     * no {@code ops} key at all. {@code fieldOf("ops")} would then fail the
     * decode on the next load, and {@code SavedDataStorage#readSavedData}
     * swallows a failed decode into "no saved data" — so the {@code grants} map
     * would vanish along with it and every settled withdrawal would re-mint on
     * replay. Strictness belongs on the LEGACY-file reader, which is a one-shot
     * import of a file some other version wrote and which cannot be re-derived
     * (see {@link #migrateLegacyStore}), not on the round-trip codec that reads
     * back what this very class just wrote.
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
     * old file is only renamed to {@code .migrated} AFTER the merge has been
     * read back off disk, so re-running it (including after a failed rename or
     * a fail-closed abort) changes nothing.
     *
     * <p><b>Why the read-back:</b> {@link #flush} returning tells us nothing
     * about whether the bytes landed. {@code SavedDataStorage#tryWrite} catches
     * the {@code IOException} from {@code NbtIo.writeCompressed} and only logs
     * it, and {@code collectDirtyTagsToSave} has already cleared the dirty flag
     * by then, so a full disk, a read-only mount or any I/O fault produces a
     * silent no-op that {@code saveAndJoin()} reports as success and {@code
     * isDirty()} cannot detect. Renaming the source away on the strength of
     * that would destroy the only copy of the ledger AND leave the new store
     * empty — the exact double-consume / double-mint this method exists to
     * prevent, with no recovery path left. So the merged entries are read back
     * from the new {@code .dat} and compared before the old file is touched.
     *
     * <p><b>Why the backup:</b> the read-back proves the write landed, but not
     * that the file survived a write that did NOT land. {@code
     * NbtIo.writeCompressed} opens the target with {@code TRUNCATE_EXISTING}
     * (verified with {@code javap}), so a write that fails partway leaves the
     * EXISTING 26.2 ledger truncated or half-written, and vanilla then reads
     * that corpse as "no saved data". Any record that lived only in the 26.2
     * file — every charge and withdrawal settled since a previous migration
     * whose rename failed — would be gone, and the legacy file cannot bring
     * those back. So the 26.2 file is copied aside before the migration's flush
     * and restored if the verification fails. This is scoped deliberately to the
     * ONE extra write this migration adds: the same truncate-in-place exposure
     * exists on every ordinary withdrawal {@link #flush} and is a property of
     * the vanilla save path, recorded there as a residual window rather than
     * worked around here.
     *
     * <p>The backup is written with {@link StandardOpenOption#SYNC}, so its
     * CONTENT is on the medium before the migration touches the target. Its
     * DIRECTORY ENTRY is not: there is no portable way to fsync a directory from
     * Java, and on Windows {@code FileChannel#force} on a directory throws. A
     * power loss in the window between creating the backup and the target write
     * completing can therefore still leave a filesystem whose metadata has not
     * caught up. That residual is stated rather than papered over.
     *
     * <p><b>Order of operations.</b> Nothing touches the in-memory store until
     * every legacy entry has been decoded and validated AND the backup is in
     * hand. That ordering matters: {@code SavedDataStorage#computeIfAbsent}
     * marks a freshly created store dirty (its {@code set} calls {@code
     * SavedData#setDirty()}), and an exception out of {@code SERVER_STARTING}
     * reaches {@code MinecraftServer#runServer}'s outer handler, which runs
     * {@code stopServer} → {@code savedDataStorage.close()} → {@code
     * saveAndJoin()}. Anything this method has already put into the store is
     * therefore written on the way down, through the plain vanilla path — no
     * backup, no verification. Validating first means there is never a
     * half-applied merge to write; taking the backup first means that even a
     * refusal to proceed (a stale backup, an unreadable target) cannot end with
     * shutdown overwriting the target unprotected.
     *
     * @throws IllegalStateException if the legacy file's existence cannot be
     *         determined; if it exists but cannot be read, does not have the
     *         expected shape, or holds an entry that is not an {@code IntTag} at
     *         or above its field's minimum; if a stale {@code .premigration}
     *         backup is in the way; if the backup cannot be taken; or if the
     *         merged entries cannot be verified on disk afterwards. Starting
     *         {@code serve()} with a ledger that is unreadable, malformed or
     *         unpersisted is more dangerous than failing to boot, so none of
     *         this is swallowed. In every failure case the legacy file is left
     *         exactly where it is. A merely ABSENT legacy file is the normal
     *         path, not an error.
     */
    public static void migrateLegacyStore(MinecraftServer server) {
        Path legacy = server.getWorldPath(LevelResource.DATA).resolve(LEGACY_FILE_NAME);
        if (!exists(legacy, LEGACY_LEDGER)) {
            // Normal operation on every boot after the first (and on new worlds).
            return;
        }

        CompoundTag data = readLedgerData(legacy, LEGACY_LEDGER);

        // "ops" is REQUIRED. 1.20.1's save(CompoundTag) unconditionally did
        // tag.put("ops", ops), so a legacy file without it is either not our ledger
        // or is damaged. Accepting it as "no charges" and then retiring the source
        // would silently discard every settled charge — after which a replayed
        // emerald.charge consumes a second time. This strictness is deliberately
        // NOT mirrored onto CODEC; see CODEC's Javadoc for why the round-trip codec
        // must stay lenient.
        CompoundTag legacyOps = requireCompound(data, "ops", legacy, LEGACY_LEDGER);

        // "grants" may legitimately be ABSENT: a world saved before
        // emerald.withdraw existed has no such key (the same case CODEC's
        // optionalFieldOf covers). Present-but-not-a-compound is still a corrupt
        // file, so that fails closed.
        CompoundTag legacyGrants = optionalCompound(data, "grants", legacy, LEGACY_LEDGER);

        // Decode and validate EVERY entry before the store is even fetched, let
        // alone mutated — see this method's "Order of operations" Javadoc for the
        // shutdown save that would otherwise write a half-applied merge.
        Map<String, Integer> legacyOpEntries =
                readEntries(legacyOps, "ops", legacy, MIN_OPS_VALUE);
        Map<String, Integer> legacyGrantEntries =
                readEntries(legacyGrants, "grants", legacy, MIN_GRANTS_VALUE);

        // Preflight the backup BEFORE the store is fetched. get() below can create
        // the store, and creating it marks it dirty, after which any throw on the
        // way to the guarded write gets the target written anyway by the shutdown
        // save (see "Order of operations" above). Refusing to proceed must not be
        // the thing that causes an unprotected write.
        //
        // Checked on every boot the legacy file is still present, NOT only when
        // something gets inserted: a run that failed to delete its backup AND
        // failed to retire the legacy file leaves all three files behind, and the
        // next boot may well insert nothing (the target already has every legacy
        // key). That is precisely the state where "how far did the last run get?"
        // is unanswerable, so it must not be the state that skips the check.
        Path dataFile = ledgerFile(server);
        Path backup = dataFile.resolveSibling(dataFile.getFileName() + BACKUP_SUFFIX);
        boolean backedUp = takeBackup(dataFile, backup, legacy);

        EmeraldOpStore store = get(server);

        // putIfAbsent, never put: the new store WINS on any op_id it already knows.
        // The state machine is monotonic (null → -1 claimed → ≥0 confirmed) and the
        // old file is by definition the older snapshot, so overwriting could push a
        // confirmed grant (≥0) back to claimed (-1) — after which the two-phase
        // logic in MoyMoyExtension would treat the withdrawal as never completed
        // and mint the emeralds a second time. The same applies to a settled charge
        // being replaced by a stale consumed amount.
        //
        // What was actually inserted is remembered, not merely counted: that set is
        // exactly what has to be proven durable below.
        Map<String, Integer> insertedOps = mergeInto(store.consumed, legacyOpEntries);
        Map<String, Integer> insertedGrants = mergeInto(store.grants, legacyGrantEntries);

        boolean moved = !insertedOps.isEmpty() || !insertedGrants.isEmpty();
        if (moved) {
            store.setDirty();
        }
        try {
            // Flush even when nothing moved: get() above may have CREATED the store
            // (computeIfAbsent's set() marks it dirty), and writing that ledger out
            // costs nothing.
            flush(server);
            if (moved) {
                verifyPersisted(dataFile, legacy, insertedOps, insertedGrants);
            }
            // Verification is skipped when nothing was inserted, and that is not a
            // shortcut: with zero insertions the store's contents are precisely what
            // SavedDataStorage read back out of the target on this very boot, so
            // they are durable by construction and this flush had nothing of ours to
            // lose.
        } catch (RuntimeException e) {
            if (backedUp) {
                restore(backup, dataFile, e);
            }
            throw e;
        }

        deleteBackup(backup, backedUp);

        LOGGER.info("Migrated legacy emerald op ledger {}: {} ops, {} grants",
                LEGACY_FILE_NAME, insertedOps.size(), insertedGrants.size());

        // Only now, with the merge read back off disk, retire the old file. A
        // failure here is not fatal: the next boot reads the same file again and the
        // put-if-absent merge above is a no-op, so we log and carry on rather than
        // refusing to start. (A leftover .migrated file from an earlier partial run
        // makes Files.move throw FileAlreadyExistsException, which lands here too.)
        Path retired = legacy.resolveSibling(LEGACY_FILE_NAME + MIGRATED_SUFFIX);
        try {
            Files.move(legacy, retired);
        } catch (IOException e) {
            LOGGER.warn("Migrated the legacy emerald op ledger but could not rename {} to {}"
                    + "; it will be re-read (harmlessly) on the next start", legacy, retired, e);
        }
    }

    /**
     * Copies the 26.2 ledger aside so a failed migration write can be undone,
     * returning whether a backup was actually taken. Touches no live state, so
     * it is safe to call — and must be called — before {@link #get} creates and
     * dirties the store.
     *
     * <p>The copy goes through {@link StandardOpenOption#SYNC} rather than
     * {@code Files.copy}: {@code Files.copy} promises only that the bytes were
     * handed to the filesystem, and this backup exists precisely to survive the
     * case where the machine stops mid-write. See {@link #migrateLegacyStore}'s
     * class of residual for what {@code SYNC} still does not cover (the
     * directory entry).
     *
     * @throws IllegalStateException if a stale backup is in the way, or the
     *         backup cannot be taken. The legacy file is never touched here.
     */
    private static boolean takeBackup(Path dataFile, Path backup, Path legacy) {
        // A backup left over from a previous run means that run died between taking
        // it and either restoring or deleting it — so which of the two files is the
        // real ledger is exactly what we cannot know. Overwriting the backup here
        // would destroy the good copy with a possibly-truncated one, so stop and let
        // a human decide.
        if (exists(backup, MIGRATED_LEDGER + " backup")) {
            LOGGER.error("A previous emerald op ledger migration left {} behind, which means it "
                    + "died between backing up {} and restoring or deleting that backup. Compare "
                    + "the two files, keep whichever is the complete ledger (the backup is the "
                    + "state from BEFORE that migration), delete the other, and restart.",
                    backup, dataFile);
            throw new IllegalStateException(
                    "Stale emerald op ledger backup in the way: " + backup);
        }

        // Back up only if there is something to lose. On the ordinary upgrade — a
        // 1.20.1 world booting on 26.2 for the first time — the 26.2 ledger does not
        // exist yet, the whole file is about to be created from the merge, and a
        // failed write leaves the legacy file untouched for the next attempt.
        //
        // Taken regardless of how many entries the merge will end up inserting: that
        // count is only known after the merge, and the merge is what dirties the
        // store. A copy on the handful of boots where the legacy file still exists
        // costs nothing worth optimising away.
        if (!exists(dataFile, MIGRATED_LEDGER)) {
            return false;
        }
        try (InputStream in = Files.newInputStream(dataFile);
                // CREATE_NEW, not CREATE: absence was just established, so a
                // collision here means something else is writing this directory and
                // we would rather fail than race it.
                OutputStream out = Files.newOutputStream(backup,
                        StandardOpenOption.CREATE_NEW,
                        StandardOpenOption.WRITE,
                        StandardOpenOption.SYNC)) {
            in.transferTo(out);
        } catch (IOException e) {
            LOGGER.error("Could not back up {} to {} before migrating the legacy emerald op "
                    + "ledger; leaving {} in place", dataFile, backup, legacy, e);
            throw new IllegalStateException(
                    "Could not back up the emerald op ledger: " + dataFile, e);
        }
        return true;
    }

    /**
     * Removes the backup once the migration has been verified. A failure here is
     * only a warning: it costs nothing now, and the legacy file is about to be
     * retired, after which the next boot returns early and never looks here. It
     * matters only if that retire ALSO fails, in which case the next boot stops
     * on {@link #takeBackup}'s stale-backup check — hence the explicit permission
     * to delete the file by hand.
     */
    private static void deleteBackup(Path backup, boolean backedUp) {
        if (!backedUp) {
            return;
        }
        try {
            Files.delete(backup);
        } catch (IOException e) {
            LOGGER.warn("Emerald op ledger migration succeeded but could not delete the "
                    + "backup {}. It is safe to delete this file by hand; leaving it may "
                    + "block a future migration.", backup, e);
        }
    }

    /**
     * Puts the backup back over the target after a failed migration write. A
     * failure to restore is attached to {@code cause} as suppressed rather than
     * replacing it — the original verification failure is what explains WHY we
     * are restoring — and the backup is deliberately left on disk either way.
     */
    private static void restore(Path backup, Path dataFile, RuntimeException cause) {
        try {
            Files.move(backup, dataFile, StandardCopyOption.REPLACE_EXISTING);
            LOGGER.error("Emerald op ledger migration failed; restored {} from {}. "
                    + "The legacy ledger is still in place, so the migration will be retried "
                    + "on the next start once the underlying problem is fixed.", dataFile, backup);
        } catch (IOException e) {
            LOGGER.error("Emerald op ledger migration failed AND the backup could not be "
                    + "restored. The pre-migration ledger is still at {}; the file at {} may be "
                    + "truncated or half-written. Restore the backup by hand before starting "
                    + "the server again.", backup, dataFile, e);
            cause.addSuppressed(e);
        }
    }

    /**
     * Where {@code SavedDataStorage} keeps this store's {@code .dat}. Resolved
     * exactly the way {@code SavedDataStorage#getDataFile} does — verified with
     * {@code javap}: {@code id.withSuffix(".dat").resolveAgainst(dataFolder)},
     * where its {@code dataFolder} is the {@code LevelResource.DATA} path the
     * {@code MinecraftServer} constructor handed it, i.e. the same path {@code
     * getWorldPath} returns here.
     */
    private static Path ledgerFile(MinecraftServer server) {
        return TYPE.id().withSuffix(".dat")
                .resolveAgainst(server.getWorldPath(LevelResource.DATA));
    }

    /**
     * Whether {@code file} exists, failing closed when that cannot be determined.
     *
     * <p>{@code Files.exists} is wrong here: it answers {@code false} both for
     * "not there" and for "could not tell" (a denied ACL on the file or a parent,
     * an I/O fault, a disconnected network share). Treating the second as the
     * first would let {@link #migrateLegacyStore} return quietly and let {@code
     * serve()} start on an EMPTY ledger while the real one sits right there —
     * settled ops would replay as fresh consumes and fresh mints. {@code
     * Files.notExists} has the same ambiguity in the other direction. So probe
     * with {@code readAttributes} and let only {@link NoSuchFileException} mean
     * "absent"; every other {@link IOException} is a refusal to answer.
     */
    private static boolean exists(Path file, String what) {
        try {
            Files.readAttributes(file, BasicFileAttributes.class);
            return true;
        } catch (NoSuchFileException e) {
            return false;
        } catch (IOException e) {
            LOGGER.error("Could not determine whether {} exists at {}; refusing to start rather "
                    + "than assume it is absent", what, file, e);
            throw new IllegalStateException(
                    "Could not determine whether " + what + " exists: " + file, e);
        }
    }

    /**
     * Reads {@code file} as one of our {@code .dat} ledgers and returns its
     * {@code data} compound. Fails closed on anything unreadable or malformed —
     * see {@link #migrateLegacyStore}'s {@code @throws} for why.
     */
    private static CompoundTag readLedgerData(Path file, String what) {
        CompoundTag root;
        try {
            // Read the raw NBT, no DataFixer: the root is
            // {data:{ops:{...},grants:{...}}, DataVersion:n} — the shape 1.20.1's
            // SavedData#save(File) wrote and the shape 26.2's
            // SavedDataStorage#encodeUnchecked still writes — and TYPE's
            // DataFixTypes.SAVED_DATA_WEATHER is registered as DSL.remainder(), a
            // pass-through, so running the fixer chain over it could not change a
            // single byte anyway.
            root = NbtIo.readCompressed(file, NbtAccounter.unlimitedHeap());
        } catch (IOException | RuntimeException e) {
            LOGGER.error("Failed to read {} at {}", what, file, e);
            throw new IllegalStateException("Failed to read " + what + ": " + file, e);
        }
        // Both writers ALWAYS wrap in "data", so its absence means this file is not
        // the ledger we think it is. Bail out loudly rather than treat a file we
        // cannot parse as an empty ledger — with the legacy file that would lose the
        // very records this migration exists to keep.
        return requireCompound(root, "data", file, what);
    }

    /**
     * A REQUIRED child compound: fails closed both when the key is missing and
     * when it holds something that is not a {@link CompoundTag}. {@code
     * CompoundTag#getCompoundOrEmpty} conflates those two with "empty", which is
     * never the right answer on a path that retires the source file.
     */
    private static CompoundTag requireCompound(
            CompoundTag parent, String field, Path file, String what) {
        if (!parent.contains(field)) {
            LOGGER.error("{} at {} has no \"{}\" compound; leaving the file in place",
                    what, file, field);
            throw new IllegalStateException(
                    what + " has no \"" + field + "\" compound: " + file);
        }
        return parent.getCompound(field).orElseThrow(() -> {
            LOGGER.error("{} at {}: \"{}\" is present but is not a compound; "
                    + "leaving the file in place", what, file, field);
            return new IllegalStateException(
                    what + ": \"" + field + "\" is present but is not a compound: " + file);
        });
    }

    /**
     * An OPTIONAL child compound: a missing key yields an empty compound, but a
     * key holding a non-compound still fails closed.
     */
    private static CompoundTag optionalCompound(
            CompoundTag parent, String field, Path file, String what) {
        if (!parent.contains(field)) {
            return new CompoundTag();
        }
        return requireCompound(parent, field, file, what);
    }

    /**
     * Decodes and validates every {@code op_id → int} of one legacy field,
     * touching no live state. Throws on the FIRST bad entry, before anything has
     * been merged anywhere.
     *
     * <p>The tag must be an {@link IntTag} specifically, not merely numeric.
     * {@code CompoundTag#getInt} unwraps any {@code NumericTag} through {@code
     * intValue()} (verified with {@code javap}), so a {@code LongTag} would be
     * silently truncated and a {@code DoubleTag} rounded — but 1.20.1 wrote these
     * with {@code putInt}, so anything else means the file is not what it claims
     * and guessing at its numbers would put invented amounts in the ledger.
     * 1.20.1's own {@code getInt} was laxer still, yielding {@code 0} for a
     * missing or wrongly-typed value; reproducing that here would write "settled,
     * consumed 0 エメ" or "confirmed, granted 0 エメ" and then delete the
     * evidence — a charge that really consumed emeralds would be re-acked as
     * {@code duplicate} with {@code settled: 0} and the backend would credit
     * nothing for emeralds the player has already lost. On a one-way import,
     * refusing to guess beats being bug-compatible.
     *
     * <p>{@code minValue} checks the state machine's own invariant and nothing
     * more: {@code ops} entries are only ever written for a real consume ({@code
     * > 0}) and {@code grants} entries are {@code -1} or a confirmed amount.
     * <b>The {@code MoyMoyExtension.MAX_AMOUNT} / {@code MAX_WITHDRAW} ceilings
     * are deliberately NOT enforced here.</b> Those are policy constants owned by
     * the request handler and may be lowered later; enforcing them on import
     * would turn a perfectly valid old record into a server that refuses to boot.
     * A lower bound is a property of what this class can ever have written; an
     * upper bound is a property of what the handler currently accepts.
     */
    private static Map<String, Integer> readEntries(
            CompoundTag source, String field, Path file, int minValue) {
        Map<String, Integer> entries = new LinkedHashMap<>();
        for (String opId : source.keySet()) {
            Tag tag = source.get(opId);
            if (!(tag instanceof IntTag intTag)) {
                String actual = tag == null ? "absent" : tag.getClass().getSimpleName();
                LOGGER.error("{} at {}: \"{}\" entry \"{}\" is {}, not an IntTag; "
                        + "leaving the file in place", LEGACY_LEDGER, file, field, opId, actual);
                throw new IllegalStateException(LEGACY_LEDGER + ": \"" + field + "\" entry \""
                        + opId + "\" is " + actual + ", not an IntTag: " + file);
            }
            int value = intTag.intValue();
            if (value < minValue) {
                LOGGER.error("{} at {}: \"{}\" entry \"{}\" is {}, below the minimum {}; "
                        + "leaving the file in place",
                        LEGACY_LEDGER, file, field, opId, value, minValue);
                throw new IllegalStateException(LEGACY_LEDGER + ": \"" + field + "\" entry \""
                        + opId + "\" is " + value + ", below the minimum " + minValue
                        + ": " + file);
            }
            entries.put(opId, value);
        }
        return entries;
    }

    /**
     * Put-if-absent every already-validated entry into {@code target}, returning
     * only what was actually inserted — the set that has to be proven durable.
     */
    private static Map<String, Integer> mergeInto(
            Map<String, Integer> target, Map<String, Integer> entries) {
        Map<String, Integer> inserted = new LinkedHashMap<>();
        for (Map.Entry<String, Integer> entry : entries.entrySet()) {
            if (target.putIfAbsent(entry.getKey(), entry.getValue()) == null) {
                inserted.put(entry.getKey(), entry.getValue());
            }
        }
        return inserted;
    }

    /**
     * Re-reads the 26.2 {@code .dat} from disk and asserts that every entry this
     * migration inserted is actually in it, with the same value. Throws if not —
     * leaving the legacy file untouched so the next boot can retry.
     *
     * <p>Only the INSERTED entries are checked, never the whole store. A field
     * nothing was inserted into may legitimately be absent from the file: {@link
     * #CODEC}'s {@code optionalFieldOf} omits a map equal to its empty default on
     * encode, so a migration that moved only grants writes no {@code ops} key at
     * all. Checking the whole store would read that omission as data loss.
     */
    private static void verifyPersisted(Path dataFile, Path legacy,
            Map<String, Integer> insertedOps, Map<String, Integer> insertedGrants) {
        CompoundTag data = readLedgerData(dataFile, MIGRATED_LEDGER);
        verifyField(data, "ops", insertedOps, dataFile, legacy);
        verifyField(data, "grants", insertedGrants, dataFile, legacy);
    }

    private static void verifyField(CompoundTag data, String field,
            Map<String, Integer> inserted, Path dataFile, Path legacy) {
        if (inserted.isEmpty()) {
            return;
        }
        // Non-empty ⇒ the codec cannot have omitted this field, so it must be there.
        CompoundTag written = requireCompound(data, field, dataFile, MIGRATED_LEDGER);
        for (Map.Entry<String, Integer> entry : inserted.entrySet()) {
            Optional<Integer> actual = written.getInt(entry.getKey());
            if (actual.isEmpty() || !actual.get().equals(entry.getValue())) {
                LOGGER.error("Emerald op ledger migration did not reach disk: {} entry \"{}\" "
                        + "should be {} in {} but is {}. The vanilla save path swallows write "
                        + "errors, so this is how a failed write surfaces. Leaving {} in place; "
                        + "fix the underlying I/O problem and restart.",
                        field, entry.getKey(), entry.getValue(), dataFile,
                        actual.map(String::valueOf).orElse("absent"), legacy);
                throw new IllegalStateException("Emerald op ledger migration did not reach disk: "
                        + field + " entry \"" + entry.getKey() + "\" in " + dataFile);
            }
        }
    }
}

package jp.houlab.mochidsuki.moymoy;

import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

import net.minecraft.nbt.CompoundTag;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.saveddata.SavedData;

/**
 * Persistent per-op idempotency for emerald charges (DEV.md consistency model).
 *
 * <p>Maps {@code op_id → consumed エメ}. A charge command is consumed at most once:
 * a replay of a settled {@code op_id} re-acks the SAME consumed amount without
 * touching the inventory again, so an at-least-once command bus + a lost ack can
 * never double-consume. Persisted as world {@link SavedData} so the claim
 * survives a server restart (the "consume after restart" double-spend guard).
 *
 * <p>Stored on the overworld's data storage under {@code moymoy_emerald_ops}.
 */
public final class EmeraldOpStore extends SavedData {

    private static final String DATA_NAME = "moymoy_emerald_ops";

    /** op_id → consumed エメ. Concurrent: written from the connector IO thread. */
    private final Map<String, Integer> consumed = new ConcurrentHashMap<>();

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
        return store;
    }

    @Override
    public CompoundTag save(CompoundTag tag) {
        CompoundTag ops = new CompoundTag();
        for (Map.Entry<String, Integer> e : new HashMap<>(consumed).entrySet()) {
            ops.putInt(e.getKey(), e.getValue());
        }
        tag.put("ops", ops);
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
}

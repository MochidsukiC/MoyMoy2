package jp.houlab.mochidsuki.moymoy;

import java.nio.charset.StandardCharsets;
import java.util.UUID;

import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.mojang.logging.LogUtils;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerPlayer;
import org.slf4j.Logger;

import jp.houlab.mochidsuki.mochi.connector.CommandDispatch;

/**
 * MoyMoy receiver extension (DEV.md §7.3) — <b>emerald-charge executor</b>.
 *
 * <p>The {@code moymoy.cs.mnn} wallet backend owns the balance. When a player
 * charges from the app, the backend sends a command over the MochiOS command bus
 * to {@code moymoy.<UUID>.minecraft.auto.mnn}; the Hub auto-routes it to the
 * player's live server, the connector relays it here as a {@code CMD_INBOUND}, and
 * this handler consumes the player's inventory emeralds and acks the consumed
 * amount. The backend credits the balance ONLY on that ack.
 *
 * <p>Commands (opaque JSON {@code data}):
 * <pre>
 * emerald.charge  {"op_id","idem_key","verb":"emerald.charge","target_uuid","amount"}
 *   ack → {"op_id","status":"ok|duplicate|unauthorized|unknown_verb|bad_request
 *                         |player_offline|insufficient_emeralds|internal_error",
 *          "settled":&lt;consumed&gt;}
 * inventory.query {"req_id","verb":"inventory.query","target_uuid"}
 *   reply → {"req_id","emeralds","blocks"}  (or "online":false when offline)
 * </pre>
 *
 * <p>Security: {@code src} is the Hub/cert-asserted backend app_id. We only accept
 * commands from the {@code moymoy} backend ({@link #ALLOWED_SRC}); any other src is
 * {@code unauthorized}.
 *
 * <p>Idempotency: {@code op_id} consumption is claimed in a persistent
 * {@link EmeraldOpStore} only after a successful consume, so a transient failure
 * (player offline / no emeralds) is retryable, and a replay of a settled op re-acks
 * the same consumed amount without consuming again — surviving a server restart.
 */
public final class MoyMoyExtension implements CommandDispatch.Handler {

    private static final Logger LOGGER = LogUtils.getLogger();

    /** The backend app_id (cert SAN) allowed to issue commands. */
    private static final String ALLOWED_SRC = "moymoy";

    /** Bounds a single charge against a hostile/buggy backend. */
    private static final int MAX_AMOUNT = 1_000_000_000;

    private final MinecraftServer server;

    public MoyMoyExtension(MinecraftServer server) {
        this.server = server;
    }

    @Override
    public void handle(String src, byte[] data, CommandDispatch.Replier reply) {
        JsonObject cmd;
        String verb;
        try {
            cmd = JsonParser.parseString(new String(data, StandardCharsets.UTF_8)).getAsJsonObject();
            verb = optString(cmd, "verb");
        } catch (RuntimeException e) {
            LOGGER.warn("moymoy: malformed command dropped from src '{}': {}", src, e.toString());
            return;
        }

        // Guard the whole flow against ANY Throwable so a single faulty command
        // cannot take down the shared connector IO thread (PiggleShop hardening).
        try {
            switch (verb) {
                case "emerald.charge" -> handleCharge(src, cmd, reply);
                case "inventory.query" -> handleInventory(src, cmd, reply);
                default -> {
                    String opId = optString(cmd, "op_id");
                    if (!opId.isEmpty()) {
                        ackCharge(reply, src, opId, "unknown_verb", 0);
                    } else {
                        LOGGER.warn("moymoy: unknown verb '{}' from src '{}'", verb, src);
                    }
                }
            }
        } catch (Throwable t) {
            LOGGER.error("moymoy: handler crashed (verb '{}', src '{}')", verb, src, t);
            try {
                String opId = optString(cmd, "op_id");
                if (!opId.isEmpty()) {
                    ackCharge(reply, src, opId, "internal_error", 0);
                }
            } catch (Throwable ackFailure) {
                LOGGER.error("moymoy: failed to ack internal_error", ackFailure);
            }
        }
    }

    // ── emerald.charge ───────────────────────────────────────────────────────

    private void handleCharge(String src, JsonObject cmd, CommandDispatch.Replier reply) {
        String opId = optString(cmd, "op_id");
        if (opId.isEmpty()) {
            LOGGER.warn("moymoy: charge without op_id from src '{}' (dropped)", src);
            return;
        }
        if (!ALLOWED_SRC.equals(src)) {
            LOGGER.warn("moymoy: rejected charge from unauthorized src '{}' (op {})", src, opId);
            ackCharge(reply, src, opId, "unauthorized", 0);
            return;
        }
        UUID uuid = parseUuid(optString(cmd, "target_uuid"));
        int amount = optInt(cmd, "amount");
        if (uuid == null || amount <= 0 || amount > MAX_AMOUNT) {
            ackCharge(reply, src, opId, "bad_request", 0);
            return;
        }

        // Replay of a settled op ⇒ re-ack the same consumed amount (no re-consume).
        EmeraldOpStore store = EmeraldOpStore.get(server);
        Integer prior = store.recorded(opId);
        if (prior != null) {
            ackCharge(reply, src, opId, "duplicate", prior);
            return;
        }

        // Consume atomically on the server thread. handle() runs on the connector
        // IO thread, so blocking on the future does not deadlock the server.
        Integer consumed = server.submit(() -> {
            ServerPlayer player = server.getPlayerList().getPlayer(uuid);
            if (player == null) {
                return null; // offline — retryable
            }
            return EmeraldUtil.consume(player, amount);
        }).join();

        if (consumed == null) {
            ackCharge(reply, src, opId, "player_offline", 0);
            return;
        }
        if (consumed <= 0) {
            // No emeralds to consume — retryable (the player may acquire some).
            ackCharge(reply, src, opId, "insufficient_emeralds", 0);
            return;
        }

        store.record(opId, consumed); // claim only after a real consume
        LOGGER.info("moymoy: charged op {} → {} consumed {} エメ", opId, uuid, consumed);
        ackCharge(reply, src, opId, "ok", consumed);
    }

    // ── inventory.query ──────────────────────────────────────────────────────

    private void handleInventory(String src, JsonObject cmd, CommandDispatch.Replier reply) {
        String reqId = optString(cmd, "req_id");
        if (reqId.isEmpty() || !ALLOWED_SRC.equals(src)) {
            return; // nothing to reply to / unauthorized — drop silently
        }
        UUID uuid = parseUuid(optString(cmd, "target_uuid"));
        if (uuid == null) {
            return;
        }
        int[] inv = server.submit(() -> {
            ServerPlayer player = server.getPlayerList().getPlayer(uuid);
            return player == null ? null : EmeraldUtil.count(player);
        }).join();

        JsonObject o = new JsonObject();
        o.addProperty("req_id", reqId);
        if (inv == null) {
            // No live player on THIS server for that UUID — the app's gameUuid does
            // not match a logged-in player (offline, or an online/offline-mode UUID
            // mismatch). Logged so a "0 emeralds" report is diagnosable.
            LOGGER.info("moymoy: inventory.query {} — no online player for that UUID "
                    + "(offline or UUID mismatch); replying online=false", uuid);
            o.addProperty("online", false);
            o.addProperty("emeralds", 0);
            o.addProperty("blocks", 0);
        } else {
            LOGGER.info("moymoy: inventory.query {} — {} emeralds + {} blocks", uuid, inv[0], inv[1]);
            o.addProperty("online", true);
            o.addProperty("emeralds", inv[0]);
            o.addProperty("blocks", inv[1]);
        }
        reply.reply(ALLOWED_SRC, o.toString().getBytes(StandardCharsets.UTF_8));
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    private static void ackCharge(CommandDispatch.Replier reply, String dst, String opId,
                                  String status, int settled) {
        JsonObject o = new JsonObject();
        o.addProperty("op_id", opId);
        o.addProperty("status", status);
        o.addProperty("settled", settled);
        reply.reply(dst, o.toString().getBytes(StandardCharsets.UTF_8));
    }

    private static UUID parseUuid(String s) {
        if (s.isEmpty()) {
            return null;
        }
        try {
            return UUID.fromString(s);
        } catch (IllegalArgumentException e) {
            return null;
        }
    }

    private static String optString(JsonObject o, String k) {
        JsonElement e = o.get(k);
        return e != null && e.isJsonPrimitive() && e.getAsJsonPrimitive().isString()
                ? e.getAsString() : "";
    }

    private static int optInt(JsonObject o, String k) {
        if (!o.has(k) || !o.get(k).isJsonPrimitive() || !o.getAsJsonPrimitive(k).isNumber()) {
            return 0;
        }
        try {
            return o.get(k).getAsBigDecimal().intValueExact();
        } catch (ArithmeticException | NumberFormatException e) {
            return 0;
        }
    }
}

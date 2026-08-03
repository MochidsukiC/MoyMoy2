package jp.houlab.mochidsuki.moymoy;

import java.nio.charset.StandardCharsets;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.mojang.logging.LogUtils;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerPlayer;
import org.slf4j.Logger;

import jp.houlab.mochidsuki.mochi.connector.net.MnnHandler;
import jp.houlab.mochidsuki.mochi.connector.net.MnnRequest;
import jp.houlab.mochidsuki.mochi.connector.net.MnnResponse;

/**
 * MoyMoy receiver extension (DEV.md §7.3) — <b>emerald-charge executor</b>.
 *
 * <p>The {@code moymoy.cs.mnn} wallet backend owns the balance. When a player
 * charges from the app, the backend sends a consume request as HTTP in MNN to
 * {@code moymoy.<exsoft_id>.mnn} — the server the player's Hub-signed
 * attestation named — the connector relays it here, and this handler consumes
 * that player's inventory emeralds and acks the consumed amount. The backend
 * credits the balance ONLY on that ack, and the ack is this request's own HTTP
 * response.
 *
 * <p>Requests (opaque JSON body, {@code POST /}):
 * <pre>
 * emerald.charge  {"op_id","idem_key","verb":"emerald.charge","target_uuid","amount"}
 *   ack → {"op_id","status":"ok|duplicate|unauthorized|unknown_verb|bad_request
 *                         |player_offline|insufficient_emeralds|internal_error",
 *          "settled":&lt;consumed&gt;}
 * inventory.query {"req_id","verb":"inventory.query","target_uuid"}
 *   reply → {"req_id","emeralds","blocks"}  (or "online":false when offline)
 * </pre>
 *
 * <h2>Who is allowed to spend a player's emeralds</h2>
 *
 * <p>This handler is registered on the <b>new</b> {@link
 * jp.houlab.mochidsuki.mochi.connector.net.MnnServer} API rather than the legacy
 * {@code CommandDispatch} one, for one reason: the legacy handler is only handed
 * {@code src}, which over MNN is the <i>serve key</i> — the name that was
 * addressed, not the party that addressed it. Comparing it to {@code "moymoy"},
 * as this class used to, is not an authorization check at all: it succeeds for
 * anyone who can reach the name, and the connector's own G1 gate only narrows
 * that to "some CS backend or connector". Any other CS backend on the Hub could
 * therefore have consumed any player's emeralds here.
 *
 * <p>{@link MnnRequest#caller()} is the Hub's authenticated statement of who
 * sent the request, stamped after stripping whatever the client sent under the
 * same name. {@link #ALLOWED_CALLER} pins it to the one backend whose ledger
 * turns a consumption into wallet balance. Everything else — another CS backend,
 * an {@code exsoft:} connector, a {@code user:} session, or an unattributed
 * {@code null} — is refused with nothing consumed.
 *
 * <p>Idempotency: {@code op_id} consumption is claimed in a persistent
 * {@link EmeraldOpStore} only after a successful consume, so a transient failure
 * (player offline / no emeralds) is retryable, and a replay of a settled op re-acks
 * the same consumed amount without consuming again — surviving a server restart.
 */
public final class MoyMoyExtension implements MnnHandler {

    private static final Logger LOGGER = LogUtils.getLogger();

    /**
     * The one caller whose consume requests are honoured.
     *
     * <p>{@code cs:} is the kind the Hub stamps for a Central-Server backend
     * ({@link MnnRequest#CALLER_KIND_CS}); {@code app.moymoy} is the identity the
     * MochiOS app-backend launcher mints for {@code app_backends/moymoy/} and
     * registers the {@code moymoy} cs-host under (MochiOS
     * {@code hub/server/src/launcher/mod.rs} → {@code app_service_name}, i.e.
     * {@code "app." + <dir name>}). That directory name is fixed by MoyMoy's own
     * {@code tools/deploy-backend.ps1}, and the wallet backend authenticates its
     * tunnel with the launcher-minted per-process identity token
     * ({@code MOCHI_SVC_IDENTITY_TOKEN}, see {@code server/moymoy-cs/src/tunnel.rs}),
     * so this is the string the Hub derives for it.
     *
     * <p>A constant rather than a config key on purpose: making it settable would
     * turn "which backend may spend my players' emeralds" into something a server
     * operator can widen by accident, and there is exactly one right answer.
     */
    private static final String ALLOWED_CALLER =
            MnnRequest.CALLER_KIND_CS + "app.moymoy";

    /** Bounds a single charge against a hostile/buggy backend. */
    private static final int MAX_AMOUNT = 1_000_000_000;

    private final MinecraftServer server;

    public MoyMoyExtension(MinecraftServer server) {
        this.server = server;
    }

    @Override
    public CompletableFuture<MnnResponse> handle(MnnRequest req) {
        JsonObject cmd;
        String verb;
        try {
            cmd = JsonParser.parseString(req.bodyAsString()).getAsJsonObject();
            verb = optString(cmd, "verb");
        } catch (RuntimeException e) {
            LOGGER.warn("moymoy: malformed request dropped from caller '{}': {}",
                    describe(req.caller()), e.toString());
            return completed(MnnResponse.status(400));
        }

        // Authorize the CALLER before anything is consumed, and before the verb
        // is even dispatched — a refusal after a consume would be no refusal.
        if (!ALLOWED_CALLER.equals(req.caller())) {
            LOGGER.warn("moymoy: refusing '{}' from caller '{}' — only the MoyMoy wallet backend "
                    + "({}) may consume a player's emeralds", verb, describe(req.caller()), ALLOWED_CALLER);
            return completed(refusal(cmd));
        }

        // Guard the whole flow against ANY Throwable so a single faulty request
        // cannot take down the shared connector IO thread (PiggleShop hardening).
        try {
            switch (verb) {
                case "emerald.charge":
                    return handleCharge(cmd);
                case "inventory.query":
                    return handleInventory(cmd);
                default: {
                    String opId = optString(cmd, "op_id");
                    if (!opId.isEmpty()) {
                        return completed(ack(opId, "unknown_verb", 0));
                    }
                    LOGGER.warn("moymoy: unknown verb '{}'", verb);
                    return completed(MnnResponse.status(400));
                }
            }
        } catch (Throwable t) {
            LOGGER.error("moymoy: handler crashed (verb '{}')", verb, t);
            String opId = optString(cmd, "op_id");
            return completed(opId.isEmpty()
                    ? MnnResponse.status(500)
                    : ack(opId, "internal_error", 0));
        }
    }

    /**
     * The answer an unauthorized caller gets.
     *
     * <p>A charge (it carries an {@code op_id}) gets a {@code 200} with the
     * ordinary {@code unauthorized} ack, because that is what makes the refusal
     * <b>terminal</b> for the ledger on the other side: a non-2xx would be read
     * as "the exchange failed with consumption unknown" and re-driven for a day
     * before being escalated for manual review, which is exactly the wrong thing
     * to say about a request that never touched an inventory.
     *
     * <p>Anything else gets a {@code 403}. An {@code inventory.query} in
     * particular must NOT get a 200 with no counts: the caller's parser reads a
     * missing {@code online} as "found, and they own nothing", turning a refusal
     * into a believable zero balance.
     */
    private static MnnResponse refusal(JsonObject cmd) {
        String opId = optString(cmd, "op_id");
        return opId.isEmpty() ? MnnResponse.status(403) : ack(opId, "unauthorized", 0);
    }

    // ── emerald.charge ───────────────────────────────────────────────────────

    private CompletableFuture<MnnResponse> handleCharge(JsonObject cmd) {
        String opId = optString(cmd, "op_id");
        if (opId.isEmpty()) {
            LOGGER.warn("moymoy: charge without op_id (dropped)");
            return completed(MnnResponse.status(400));
        }
        UUID uuid = parseUuid(optString(cmd, "target_uuid"));
        int amount = optInt(cmd, "amount");
        if (uuid == null || amount <= 0 || amount > MAX_AMOUNT) {
            return completed(ack(opId, "bad_request", 0));
        }

        // Replay of a settled op ⇒ re-ack the same consumed amount (no re-consume).
        EmeraldOpStore store = EmeraldOpStore.get(server);
        Integer prior = store.recorded(opId);
        if (prior != null) {
            return completed(ack(opId, "duplicate", prior));
        }

        // Consume atomically on the server thread. Returning the future instead
        // of joining on it keeps the connector IO thread free (MnnHandler's whole
        // reason for being async) — the answer is sent once the hop completes.
        return server.submit(() -> {
            ServerPlayer player = server.getPlayerList().getPlayer(uuid);
            if (player == null) {
                return null; // offline — retryable
            }
            return EmeraldUtil.consume(player, amount);
        }).thenApply(consumed -> {
            if (consumed == null) {
                return ack(opId, "player_offline", 0);
            }
            if (consumed <= 0) {
                // No emeralds to consume — retryable (the player may acquire some).
                return ack(opId, "insufficient_emeralds", 0);
            }
            store.record(opId, consumed); // claim only after a real consume
            LOGGER.info("moymoy: charged op {} → {} consumed {} エメ", opId, uuid, consumed);
            return ack(opId, "ok", consumed);
        });
    }

    // ── inventory.query ──────────────────────────────────────────────────────

    private CompletableFuture<MnnResponse> handleInventory(JsonObject cmd) {
        String reqId = optString(cmd, "req_id");
        if (reqId.isEmpty()) {
            return completed(MnnResponse.status(400));
        }
        UUID uuid = parseUuid(optString(cmd, "target_uuid"));
        if (uuid == null) {
            return completed(MnnResponse.status(400));
        }
        return server.submit(() -> {
            ServerPlayer player = server.getPlayerList().getPlayer(uuid);
            return player == null ? null : EmeraldUtil.count(player);
        }).thenApply(inv -> {
            JsonObject o = new JsonObject();
            o.addProperty("req_id", reqId);
            if (inv == null) {
                // No live player on THIS server for that UUID — the attested
                // character is not logged in here (they moved servers, or logged
                // out). Logged so a "0 emeralds" report is diagnosable.
                LOGGER.info("moymoy: inventory.query {} — no online player for that UUID; "
                        + "replying online=false", uuid);
                o.addProperty("online", false);
                o.addProperty("emeralds", 0);
                o.addProperty("blocks", 0);
            } else {
                LOGGER.info("moymoy: inventory.query {} — {} emeralds + {} blocks", uuid, inv[0], inv[1]);
                o.addProperty("online", true);
                o.addProperty("emeralds", inv[0]);
                o.addProperty("blocks", inv[1]);
            }
            return MnnResponse.json(o.toString());
        });
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    /** The settlement ack, as this request's own HTTP response. */
    private static MnnResponse ack(String opId, String status, int settled) {
        JsonObject o = new JsonObject();
        o.addProperty("op_id", opId);
        o.addProperty("status", status);
        o.addProperty("settled", settled);
        return MnnResponse.json(o.toString());
    }

    private static CompletableFuture<MnnResponse> completed(MnnResponse resp) {
        return CompletableFuture.completedFuture(resp);
    }

    /** A caller for a log line — {@code null} means the Hub vouched for nobody. */
    private static String describe(String caller) {
        return caller == null ? "<unattributed>" : caller;
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

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
 * emerald.withdraw {"op_id","idem_key","verb":"emerald.withdraw","target_uuid","amount"}
 *   ack → {"op_id","status":"ok|duplicate|unknown|player_offline|bad_request
 *                         |unauthorized|internal_error",
 *          "granted":&lt;granted&gt;}
 * inventory.query {"req_id","verb":"inventory.query","target_uuid"}
 *   reply → {"req_id","emeralds","blocks"}  (or "online":false when offline)
 * </pre>
 *
 * <h2>Why emerald.withdraw is claim-then-confirm, not consume-then-record</h2>
 *
 * <p>{@code emerald.charge} consumes inventory emeralds THEN records the op —
 * if the server crashes before recording, a replay just re-consumes (already
 * gone, so this replays as {@code insufficient_emeralds} or a genuine second
 * consume — either way, no emeralds are created from nothing). {@code
 * emerald.withdraw} mints emeralds that did not previously exist, so the same
 * order would let a crash-and-replay double-mint. Instead a withdrawal is
 * two-phase, entirely inside one server-thread hop:
 * <ol>
 *   <li>Check {@link EmeraldOpStore#grantState}: confirmed → re-ack the same
 *       amount ({@code duplicate}); claimed-but-unconfirmed → {@code unknown}
 *       (a prior crash happened between steps 2 and 4 below — the backend must
 *       NOT refund blindly, because the grant may have actually happened).
 *   <li>{@link EmeraldOpStore#claimGrant} the op, then {@link
 *       EmeraldOpStore#flush} it to disk BEFORE minting anything.
 *   <li>{@link EmeraldUtil#grant grant} the emeralds.
 *   <li>{@link EmeraldOpStore#recordGrant} the confirmed amount, then {@link
 *       EmeraldOpStore#flush} again.
 * </ol>
 * A crash between steps 2 and 4 leaves the claim on disk as {@code -1}
 * (unconfirmed) — that is the honest, reportable {@code unknown} outcome the
 * design accepts rather than guessing either way.
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

    /**
     * Bounds a single withdrawal to one inventory of emerald blocks (36 slots ×
     * 64 = 2,304 blocks = 20,736 エメ). The wallet backend enforces its own limit
     * too, but this mod enforces it independently — same reasoning as {@link
     * #MAX_AMOUNT}: a compromised or buggy backend must not be able to make this
     * mod itself spin the server thread minting an unbounded number of stacks.
     */
    private static final int MAX_WITHDRAW = 20_736;

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
                case "emerald.withdraw":
                    return handleWithdraw(cmd);
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
            if (opId.isEmpty()) {
                return completed(MnnResponse.status(500));
            }
            // A throw from handleWithdraw itself (before it hands off to
            // server.submit) still needs the "granted" field, not "settled" — see
            // grantAck's Javadoc for why the two must never be confused.
            return completed("emerald.withdraw".equals(verb)
                    ? grantAck(opId, "internal_error", 0)
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
     *
     * <p>This runs BEFORE the verb switch, so it reads {@code verb} out of
     * {@code cmd} itself to pick the right ack shape: a withdrawal's ack field
     * is {@code granted}, not {@code settled} — see {@link #grantAck}.
     */
    private static MnnResponse refusal(JsonObject cmd) {
        String opId = optString(cmd, "op_id");
        if (opId.isEmpty()) {
            return MnnResponse.status(403);
        }
        return "emerald.withdraw".equals(optString(cmd, "verb"))
                ? grantAck(opId, "unauthorized", 0)
                : ack(opId, "unauthorized", 0);
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

    // ── emerald.withdraw ─────────────────────────────────────────────────────

    private CompletableFuture<MnnResponse> handleWithdraw(JsonObject cmd) {
        String opId = optString(cmd, "op_id");
        if (opId.isEmpty()) {
            LOGGER.warn("moymoy: withdraw without op_id (dropped)");
            return completed(MnnResponse.status(400));
        }
        UUID uuid = parseUuid(optString(cmd, "target_uuid"));
        int amount = optInt(cmd, "amount");
        if (uuid == null || amount <= 0 || amount > MAX_WITHDRAW) {
            return completed(grantAck(opId, "bad_request", 0));
        }

        // The ENTIRE claim → grant → confirm sequence runs inside this ONE
        // server.submit — deliberately not a check on the IO thread followed by
        // a submit (that's how emerald.charge does it, and it's fine there
        // because a double-consume only wastes what the player already had).
        // Two concurrent withdrawals for the same op_id must never both observe
        // "not yet claimed": since server.submit serializes onto the single
        // server thread, doing the whole check-claim-grant-confirm dance in one
        // Supplier is what makes that structurally impossible rather than merely
        // unlikely.
        return server.submit(() -> {
            EmeraldOpStore store = EmeraldOpStore.get(server);
            boolean claimed = false;
            try {
                Integer state = store.grantState(opId);
                if (state != null) {
                    // ≥0: settled — re-ack the same amount, touch nothing. -1:
                    // claimed but never confirmed (a prior crash between claim and
                    // confirm) — report it honestly as unknown rather than guess.
                    return state >= 0 ? grantAck(opId, "duplicate", state) : grantAck(opId, "unknown", 0);
                }

                ServerPlayer player = server.getPlayerList().getPlayer(uuid);
                if (player == null) {
                    // Nothing claimed yet, so nothing to unwind — safe to let the
                    // backend refund and retry once the player is back online.
                    return grantAck(opId, "player_offline", 0);
                }

                if (!store.claimGrant(opId)) {
                    // Structurally unreachable here (single server thread; state
                    // was just observed null above with no other mutator in
                    // between) — but never silently drop a failed claim. Re-read
                    // and answer from whatever it actually settled to instead of
                    // assuming and possibly double-granting.
                    Integer raced = store.grantState(opId);
                    return raced != null && raced >= 0
                            ? grantAck(opId, "duplicate", raced)
                            : grantAck(opId, "unknown", 0);
                }
                // Flush the claim to disk BEFORE minting a single emerald: if the
                // server dies right after this, restart sees state==-1 (unknown)
                // instead of forgetting the claim entirely and re-granting on
                // replay — the exact double-mint this whole design prevents.
                claimed = true;
                EmeraldOpStore.flush(server);

                EmeraldUtil.grant(player, amount);
                store.recordGrant(opId, amount);
                EmeraldOpStore.flush(server);

                LOGGER.info("moymoy: withdrew op {} → {} granted {} エメ", opId, uuid, amount);
                return grantAck(opId, "ok", amount);
            } catch (Throwable t) {
                // A throw from INSIDE this Supplier completes the returned
                // CompletableFuture exceptionally, which the outer catch in
                // handle() never sees (it only wraps the synchronous call into
                // handleWithdraw, not this server-thread hop) — so without this,
                // a crash here would surface as a connector-level failure instead
                // of an ack, which is precisely what handle()'s own Throwable
                // guard exists to avoid.
                LOGGER.error("moymoy: withdraw op {} crashed for {}", opId, uuid, t);
                // If the claim never reached disk, nothing happened at all —
                // internal_error (retryable, refundable). If it did, the on-disk
                // state is already -1 (claimed, unconfirmed), so answer "unknown"
                // to agree with what a retry would read back, rather than
                // promising a refund the backend might issue alongside emeralds
                // that were, in fact, minted.
                return claimed ? grantAck(opId, "unknown", 0) : grantAck(opId, "internal_error", 0);
            }
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

    /**
     * The withdrawal ack, as this request's own HTTP response. Uses the field
     * name {@code granted} — deliberately NOT {@code settled} (the charge ack's
     * field, {@link #ack}) — so a caller can never misread a withdrawal ack as a
     * charge ack (or vice versa) by field-matching alone.
     */
    private static MnnResponse grantAck(String opId, String status, int granted) {
        JsonObject o = new JsonObject();
        o.addProperty("op_id", opId);
        o.addProperty("status", status);
        o.addProperty("granted", granted);
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

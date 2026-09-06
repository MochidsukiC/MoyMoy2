package jp.houlab.mochidsuki.moymoy;

import com.mojang.logging.LogUtils;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.minecraft.commands.Commands;
import net.minecraft.network.chat.Component;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerPlayer;
import org.slf4j.Logger;

import jp.houlab.mochidsuki.mochi.MochiMod;

/**
 * MoyMoy — Fabric 26.2 entry point.
 *
 * <p>A MochiOS connector <b>receiver extension</b> (DEV.md §7.3): the in-world
 * emerald-charge executor for the {@code moymoy} app. The {@code moymoy.cs.mnn}
 * wallet backend owns the balance; on an app charge it sends a consume request as
 * HTTP in MNN to {@code moymoy.<exsoft_id>.mnn} — the server named by the
 * Hub-signed attestation the player approved — the connector relays it to
 * {@link MoyMoyExtension}, and that consumes the inventory emeralds.
 *
 * <p>Depends on the MochiOS connector mod ({@code mochi}) for
 * {@link MochiMod#MNN_SERVER}. The connector advertises every served key to the
 * Hub, so hosting {@code moymoy} needs no per-app config: just load this jar next
 * to the {@code mochi} connector mod and set a non-empty {@code mcserver_id} in
 * {@code mochi-server.toml} (empty ⇒ connector won't start).
 *
 * <p>Also registers a read-only {@code /eme} command that shows the player's
 * chargeable inventory (no backend call — the app's charge tab performs the
 * actual charge).
 *
 * <h2>Why the {@code main} entrypoint, not {@code server}</h2>
 *
 * <p>{@code fabric.mod.json} lists this class under {@code "main"} with
 * {@code "environment": "*"}, deliberately NOT under {@code "server"} /
 * {@code DedicatedServerModInitializer}: those run only on a dedicated server, so
 * on an integrated (single-player / LAN-opened) server the emerald-charge
 * executor would silently never be served — the app's charge tab would fail
 * against a world that is otherwise running perfectly well. Forge's {@code @Mod}
 * constructor, which this class used to be, ran on both sides, so {@code "main"}
 * plus {@code "environment": "*"} is the faithful port of it. The {@code mochi}
 * connector mod itself is registered exactly the same way.
 */
public final class MoyMoyMod implements ModInitializer {

    public static final String MOD_ID = "moymoy";

    /** The connector app_id this mod hosts (DEV.md §7.3.4). */
    public static final String APP_ID = "moymoy";

    public static final Logger LOGGER = LogUtils.getLogger();

    @Override
    public void onInitialize() {
        ServerLifecycleEvents.SERVER_STARTING.register(this::onServerStarting);
        CommandRegistrationCallback.EVENT.register((dispatcher, buildContext, selection) -> {
            dispatcher.register(Commands.literal("eme").executes(ctx -> {
                ServerPlayer player = ctx.getSource().getPlayerOrException();
                int[] inv = EmeraldUtil.count(player);
                int eme = inv[0] + inv[1] * 9;
                ctx.getSource().sendSuccess(() -> Component.literal(
                        "§a[MoyMoy]§r 手持ち " + inv[0] + " エメラルド ＋ " + inv[1] + " ブロック = §a"
                                + eme + " エメ§r 換金可能。MoyMoy アプリの「チャージ」から換金できます。"),
                        false);
                return eme;
            }));
        });
        LOGGER.info("MoyMoy mod constructing (app_id={})", APP_ID);
    }

    /**
     * Serve the emerald-charge executor on the shared connector's in-JVM MNN
     * server. The {@code mochi} mod owns {@link MochiMod#MNN_SERVER}; we only add
     * our serve key, which the connector then advertises to the Hub.
     *
     * <p>Fabric has no equivalent of Forge's {@code mods.toml}
     * {@code ordering=AFTER}, and none is needed: {@link MochiMod#MNN_SERVER} is
     * a {@code static final} field initialised eagerly to a plain
     * {@code new MnnServer()}, so it is non-null the moment the class loads no
     * matter which mod initialises first, and {@code MnnServer.serve} fires the
     * connector's {@code setOnChange} hook, which re-advertises
     * {@code served_hosts} in HELLO. A key added before the connector wires
     * itself in is therefore advertised when it does; one added after is
     * advertised immediately.
     *
     * <p>The <b>new</b> API, not the legacy {@code CommandDispatch} one, because
     * only this one carries {@code MnnRequest.caller()} — the Hub's authenticated
     * statement of who sent the request. Without it the extension cannot tell the
     * MoyMoy wallet backend from any other backend that can reach the name, and
     * an emerald consume must not be open to "any backend" (see
     * {@link MoyMoyExtension}).
     */
    private void onServerStarting(MinecraftServer server) {
        // Migrate the on-disk ledger BEFORE serving, and let it run to completion
        // first. serve() publishes the name, and from that instant a charge or a
        // withdrawal can arrive; if the legacy <world>/data/moymoy_emerald_ops.dat
        // has not yet been carried over to the 26.2 store, an already-settled
        // op_id reads back as unrecorded — which replays a settled charge as a
        // fresh consume, and a settled withdrawal as a fresh mint. Preventing
        // exactly that is the op store's entire job, so it must be whole before
        // the first request is accepted, not merely before the second.
        EmeraldOpStore.migrateLegacyStore(server);

        MochiMod.MNN_SERVER.serve(APP_ID, new MoyMoyExtension(server));
        LOGGER.info("MoyMoy: emerald-charge executor serving '{}' over MNN "
                + "(the connector advertises it to the Hub; ensure mcserver_id is set).",
                APP_ID);
    }
}

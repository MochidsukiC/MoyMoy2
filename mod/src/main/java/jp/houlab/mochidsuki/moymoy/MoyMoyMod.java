package jp.houlab.mochidsuki.moymoy;

import com.mojang.brigadier.CommandDispatcher;
import com.mojang.logging.LogUtils;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;
import net.minecraft.network.chat.Component;
import net.minecraft.server.level.ServerPlayer;
import net.minecraftforge.common.MinecraftForge;
import net.minecraftforge.event.RegisterCommandsEvent;
import net.minecraftforge.event.server.ServerStartingEvent;
import net.minecraftforge.eventbus.api.SubscribeEvent;
import net.minecraftforge.fml.common.Mod;
import org.slf4j.Logger;

import jp.houlab.mochidsuki.mochi.MochiMod;

/**
 * MoyMoy — Forge 1.20.1 entry point.
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
 */
@Mod(MoyMoyMod.MOD_ID)
public final class MoyMoyMod {

    public static final String MOD_ID = "moymoy";

    /** The connector app_id this mod hosts (DEV.md §7.3.4). */
    public static final String APP_ID = "moymoy";

    public static final Logger LOGGER = LogUtils.getLogger();

    public MoyMoyMod() {
        MinecraftForge.EVENT_BUS.register(this);
        LOGGER.info("MoyMoy mod constructing (app_id={})", APP_ID);
    }

    /**
     * Serve the emerald-charge executor on the shared connector's in-JVM MNN
     * server. The {@code mochi} mod loads first (mods.toml {@code ordering=AFTER})
     * and owns {@link MochiMod#MNN_SERVER}; we only add our serve key, which the
     * connector then advertises to the Hub.
     *
     * <p>The <b>new</b> API, not the legacy {@code CommandDispatch} one, because
     * only this one carries {@code MnnRequest.caller()} — the Hub's authenticated
     * statement of who sent the request. Without it the extension cannot tell the
     * MoyMoy wallet backend from any other backend that can reach the name, and
     * an emerald consume must not be open to "any backend" (see
     * {@link MoyMoyExtension}).
     */
    @SubscribeEvent
    public void onServerStarting(ServerStartingEvent event) {
        MochiMod.MNN_SERVER.serve(APP_ID, new MoyMoyExtension(event.getServer()));
        LOGGER.info("MoyMoy: emerald-charge executor serving '{}' over MNN "
                + "(the connector advertises it to the Hub; ensure mcserver_id is set).",
                APP_ID);
    }

    @SubscribeEvent
    public void onRegisterCommands(RegisterCommandsEvent event) {
        CommandDispatcher<CommandSourceStack> d = event.getDispatcher();
        d.register(Commands.literal("eme").executes(ctx -> {
            ServerPlayer player = ctx.getSource().getPlayerOrException();
            int[] inv = EmeraldUtil.count(player);
            int eme = inv[0] + inv[1] * 9;
            ctx.getSource().sendSuccess(() -> Component.literal(
                    "§a[MoyMoy]§r 手持ち " + inv[0] + " エメラルド ＋ " + inv[1] + " ブロック = §a"
                            + eme + " エメ§r 換金可能。MoyMoy アプリの「チャージ」から換金できます。"),
                    false);
            return eme;
        }));
    }
}

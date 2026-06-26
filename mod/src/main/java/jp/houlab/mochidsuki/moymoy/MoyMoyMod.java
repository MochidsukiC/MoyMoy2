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
 * wallet backend owns the balance; on an app charge it sends a consume command
 * over the command bus to {@code moymoy.<UUID>.minecraft.auto.mnn}, the Hub
 * auto-routes it to the player's live server, the connector relays it as a
 * {@code CMD_INBOUND}, and {@link MoyMoyExtension} consumes the inventory emeralds.
 *
 * <p>Depends on the MochiOS connector mod ({@code mochi}) for {@link MochiMod#DISPATCH}.
 * Add {@code "moymoy"} to {@code mochi-server.toml [connector].hosted_app_ids} so
 * the connector advertises the app to the Hub.
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
     * Register the emerald-charge executor into the shared connector dispatch. The
     * {@code mochi} mod loads first (mods.toml {@code ordering=AFTER}) and owns
     * {@link MochiMod#DISPATCH}; we only add our app_id handler. Dispatch is keyed
     * by app_id at command time, so registering here is sufficient.
     */
    @SubscribeEvent
    public void onServerStarting(ServerStartingEvent event) {
        MochiMod.DISPATCH.register(APP_ID, new MoyMoyExtension(event.getServer()));
        LOGGER.info("MoyMoy: emerald-charge executor registered for app_id '{}'. "
                + "Ensure mochi-server.toml [connector].hosted_app_ids contains \"{}\".",
                APP_ID, APP_ID);
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

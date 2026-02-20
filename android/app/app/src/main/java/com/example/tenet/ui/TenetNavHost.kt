package com.example.tenet.ui

import androidx.compose.runtime.Composable
import androidx.hilt.navigation.compose.hiltViewModel
import androidx.navigation.NavHostController
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import com.example.tenet.ui.setup.SetupScreen
import com.example.tenet.ui.timeline.TimelineScreen
import com.example.tenet.ui.timeline.TimelineViewModel

/**
 * Top-level navigation graph for the Tenet app.
 *
 * Phase 1 exposes two destinations:
 *  - "setup"    — first-run identity creation and relay URL entry
 *  - "timeline" — read-only public message feed
 *
 * Phase 2+ will add: compose, conversations, peers, friends, groups,
 * profile, notifications, and settings screens.
 */
@Composable
fun TenetNavHost(navController: NavHostController) {
    NavHost(navController = navController, startDestination = Routes.SETUP) {
        composable(Routes.SETUP) {
            SetupScreen(
                onSetupComplete = {
                    navController.navigate(Routes.TIMELINE) {
                        popUpTo(Routes.SETUP) { inclusive = true }
                    }
                }
            )
        }

        composable(Routes.TIMELINE) {
            val viewModel: TimelineViewModel = hiltViewModel()
            TimelineScreen(viewModel = viewModel)
        }
    }
}

object Routes {
    const val SETUP = "setup"
    const val TIMELINE = "timeline"
    // Phase 2+ routes (placeholders)
    const val COMPOSE = "compose"
    const val CONVERSATIONS = "conversations"
    const val CONVERSATION_DETAIL = "conversation/{peerId}"
    const val PEERS = "peers"
    const val PEER_DETAIL = "peer/{peerId}"
    const val FRIENDS = "friends"
    const val GROUPS = "groups"
    const val GROUP_DETAIL = "group/{groupId}"
    const val PROFILE = "profile"
    const val NOTIFICATIONS = "notifications"
    const val SETTINGS = "settings"
}

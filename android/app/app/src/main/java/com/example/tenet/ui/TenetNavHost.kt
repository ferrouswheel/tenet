package com.example.tenet.ui

import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Chat
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.People
import androidx.compose.material.icons.filled.Person
import androidx.compose.material.icons.filled.PersonAdd
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.unit.dp
import androidx.hilt.navigation.compose.hiltViewModel
import androidx.navigation.NavHostController
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.navArgument
import com.example.tenet.ui.compose.ComposeScreen
import com.example.tenet.ui.conversations.ConversationDetailScreen
import com.example.tenet.ui.conversations.ConversationsScreen
import com.example.tenet.ui.friends.FriendsScreen
import com.example.tenet.ui.groups.GroupDetailScreen
import com.example.tenet.ui.groups.GroupsScreen
import com.example.tenet.ui.peers.PeerDetailScreen
import com.example.tenet.ui.peers.PeersScreen
import com.example.tenet.ui.postdetail.PostDetailScreen
import com.example.tenet.ui.profile.ProfileScreen
import com.example.tenet.ui.setup.SetupScreen
import com.example.tenet.ui.timeline.TimelineScreen
import com.example.tenet.ui.timeline.TimelineViewModel

private val BOTTOM_NAV_ROUTES = setOf(
    Routes.TIMELINE,
    Routes.CONVERSATIONS,
    Routes.PEERS,
    Routes.FRIENDS,
    Routes.PROFILE,
)

/**
 * Top-level navigation graph for the Tenet app.
 *
 * The outer [Scaffold] owns the [NavigationBar] that is shared between the five
 * top-level destinations.  Detail screens are full-screen and hide the bottom nav.
 *
 * Phase 3 additions: Peers, Friends, Groups, Profile destinations + detail routes.
 */
@Composable
fun TenetNavHost(navController: NavHostController) {
    val backStackEntry by navController.currentBackStackEntryAsState()
    val currentRoute = backStackEntry?.destination?.route
    val showBottomNav = currentRoute in BOTTOM_NAV_ROUTES

    Scaffold(
        bottomBar = {
            if (showBottomNav) {
                NavigationBar(tonalElevation = 3.dp) {
                    NavigationBarItem(
                        icon = { Icon(Icons.Default.Home, contentDescription = null) },
                        label = { Text("Timeline") },
                        selected = currentRoute == Routes.TIMELINE,
                        onClick = {
                            navController.navigate(Routes.TIMELINE) {
                                popUpTo(Routes.TIMELINE) { inclusive = true }
                                launchSingleTop = true
                            }
                        },
                    )
                    NavigationBarItem(
                        icon = {
                            Icon(Icons.AutoMirrored.Filled.Chat, contentDescription = null)
                        },
                        label = { Text("Messages") },
                        selected = currentRoute == Routes.CONVERSATIONS,
                        onClick = {
                            navController.navigate(Routes.CONVERSATIONS) {
                                popUpTo(Routes.TIMELINE) { saveState = true }
                                launchSingleTop = true
                                restoreState = true
                            }
                        },
                    )
                    NavigationBarItem(
                        icon = { Icon(Icons.Default.People, contentDescription = null) },
                        label = { Text("Peers") },
                        selected = currentRoute == Routes.PEERS,
                        onClick = {
                            navController.navigate(Routes.PEERS) {
                                popUpTo(Routes.TIMELINE) { saveState = true }
                                launchSingleTop = true
                                restoreState = true
                            }
                        },
                    )
                    NavigationBarItem(
                        icon = { Icon(Icons.Default.PersonAdd, contentDescription = null) },
                        label = { Text("Friends") },
                        selected = currentRoute == Routes.FRIENDS,
                        onClick = {
                            navController.navigate(Routes.FRIENDS) {
                                popUpTo(Routes.TIMELINE) { saveState = true }
                                launchSingleTop = true
                                restoreState = true
                            }
                        },
                    )
                    NavigationBarItem(
                        icon = { Icon(Icons.Default.Person, contentDescription = null) },
                        label = { Text("Profile") },
                        selected = currentRoute == Routes.PROFILE,
                        onClick = {
                            navController.navigate(Routes.PROFILE) {
                                popUpTo(Routes.TIMELINE) { saveState = true }
                                launchSingleTop = true
                                restoreState = true
                            }
                        },
                    )
                }
            }
        },
    ) { _ ->
        // innerPadding provides bottom offset for the NavigationBar; each
        // inner screen's own TopAppBar handles the status-bar inset.
        NavHost(
            navController = navController,
            startDestination = Routes.SETUP,
        ) {
            composable(Routes.SETUP) {
                SetupScreen(
                    onSetupComplete = {
                        navController.navigate(Routes.TIMELINE) {
                            popUpTo(Routes.SETUP) { inclusive = true }
                        }
                    },
                )
            }

            // --- Top-level bottom-nav destinations ---

            composable(Routes.TIMELINE) {
                val viewModel: TimelineViewModel = hiltViewModel()
                TimelineScreen(
                    viewModel = viewModel,
                    onComposeClick = { navController.navigate(Routes.COMPOSE) },
                    onMessageClick = { messageId ->
                        navController.navigate("${Routes.POST_BASE}/$messageId")
                    },
                )
            }

            composable(Routes.CONVERSATIONS) {
                ConversationsScreen(
                    onConversationClick = { peerId ->
                        navController.navigate("${Routes.CONVERSATION_BASE}/$peerId")
                    },
                )
            }

            composable(Routes.PEERS) {
                PeersScreen(
                    onPeerClick = { peerId ->
                        navController.navigate("${Routes.PEER_BASE}/$peerId")
                    },
                )
            }

            composable(Routes.FRIENDS) {
                FriendsScreen(
                    onPeerClick = { peerId ->
                        navController.navigate("${Routes.PEER_BASE}/$peerId")
                    },
                    onGroupsClick = { navController.navigate(Routes.GROUPS) },
                )
            }

            composable(Routes.GROUPS) {
                GroupsScreen(
                    onGroupClick = { groupId ->
                        navController.navigate("${Routes.GROUP_BASE}/$groupId")
                    },
                )
            }

            composable(Routes.PROFILE) {
                ProfileScreen()
            }

            // --- Detail screens (full-screen, no bottom nav) ---

            composable(Routes.COMPOSE) {
                ComposeScreen(onDone = { navController.popBackStack() })
            }

            composable(
                route = Routes.CONVERSATION_DETAIL,
                arguments = listOf(navArgument("peerId") { type = NavType.StringType }),
            ) {
                ConversationDetailScreen(onBack = { navController.popBackStack() })
            }

            composable(
                route = Routes.POST_DETAIL,
                arguments = listOf(navArgument("messageId") { type = NavType.StringType }),
            ) {
                PostDetailScreen(onBack = { navController.popBackStack() })
            }

            composable(
                route = Routes.PEER_DETAIL,
                arguments = listOf(navArgument("peerId") { type = NavType.StringType }),
            ) {
                PeerDetailScreen(
                    onBack = { navController.popBackStack() },
                    onSendMessage = { peerId ->
                        navController.navigate("${Routes.CONVERSATION_BASE}/$peerId")
                    },
                )
            }

            composable(
                route = Routes.GROUP_DETAIL,
                arguments = listOf(navArgument("groupId") { type = NavType.StringType }),
            ) {
                GroupDetailScreen(onBack = { navController.popBackStack() })
            }
        }
    }
}

object Routes {
    const val SETUP = "setup"
    const val TIMELINE = "timeline"
    const val CONVERSATIONS = "conversations"
    const val PEERS = "peers"
    const val FRIENDS = "friends"
    const val GROUPS = "groups"
    const val PROFILE = "profile"
    const val COMPOSE = "compose"

    // Detail routes — use base + id for navigation, full pattern for composable()
    const val CONVERSATION_BASE = "conversation"
    const val CONVERSATION_DETAIL = "conversation/{peerId}"
    const val POST_BASE = "post"
    const val POST_DETAIL = "post/{messageId}"
    const val PEER_BASE = "peer"
    const val PEER_DETAIL = "peer/{peerId}"
    const val GROUP_BASE = "group"
    const val GROUP_DETAIL = "group/{groupId}"
}

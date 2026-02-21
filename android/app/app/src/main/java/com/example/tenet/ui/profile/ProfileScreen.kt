package com.example.tenet.ui.profile

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.hilt.navigation.compose.hiltViewModel

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileScreen(
    viewModel: ProfileViewModel = hiltViewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val snackbarHostState = remember { SnackbarHostState() }

    // Edit-form fields — seeded from profile when editing begins.
    var editDisplayName by rememberSaveable { mutableStateOf("") }
    var editBio by rememberSaveable { mutableStateOf("") }

    LaunchedEffect(state.error) {
        state.error?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }
    LaunchedEffect(state.saveSuccess) {
        if (state.saveSuccess) {
            snackbarHostState.showSnackbar("Profile saved")
            viewModel.clearStatus()
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Profile") },
                actions = {
                    if (!state.isEditing && !state.isLoading) {
                        IconButton(onClick = {
                            editDisplayName = state.profile?.displayName ?: ""
                            editBio = state.profile?.bio ?: ""
                            viewModel.startEditing()
                        }) {
                            Icon(Icons.Default.Edit, contentDescription = "Edit profile")
                        }
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            if (state.isLoading) {
                CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
                return@Box
            }

            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .verticalScroll(rememberScrollState())
                    .padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                // --- Peer ID (always read-only) ---
                Text("Peer ID", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
                Text(
                    state.myPeerId,
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = FontFamily.Monospace,
                )
                HorizontalDivider()

                if (state.isEditing) {
                    // --- Edit form ---
                    OutlinedTextField(
                        value = editDisplayName,
                        onValueChange = { editDisplayName = it },
                        label = { Text("Display name") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    OutlinedTextField(
                        value = editBio,
                        onValueChange = { editBio = it },
                        label = { Text("Bio") },
                        modifier = Modifier.fillMaxWidth(),
                        minLines = 3,
                        maxLines = 6,
                    )
                    Spacer(Modifier.height(4.dp))
                    Button(
                        onClick = { viewModel.saveProfile(editDisplayName, editBio) },
                        enabled = !state.isSaving,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        if (state.isSaving) {
                            CircularProgressIndicator(
                                modifier = Modifier.height(18.dp),
                                strokeWidth = 2.dp,
                            )
                        } else {
                            Text("Save")
                        }
                    }
                    OutlinedButton(
                        onClick = viewModel::cancelEditing,
                        modifier = Modifier.fillMaxWidth(),
                    ) { Text("Cancel") }
                } else {
                    // --- Read-only view ---
                    val profile = state.profile
                    if (profile == null) {
                        Text(
                            "No profile set yet. Tap the edit button to add your display name and bio.",
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    } else {
                        if (profile.displayName != null) {
                            ProfileField("Display name", profile.displayName)
                        }
                        if (profile.bio != null) {
                            ProfileField("Bio", profile.bio)
                        }
                        if (profile.displayName == null && profile.bio == null) {
                            Text(
                                "Profile is empty. Tap the edit button to fill it in.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun ProfileField(label: String, value: String) {
    Column {
        Text(
            text = label,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.primary,
        )
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

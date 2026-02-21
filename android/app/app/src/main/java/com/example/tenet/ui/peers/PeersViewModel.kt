package com.example.tenet.ui.peers

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiPeer
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class PeersUiState(
    val peers: List<FfiPeer> = emptyList(),
    val isLoading: Boolean = false,
    val error: String? = null,
    val showAddDialog: Boolean = false,
)

@HiltViewModel
class PeersViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(PeersUiState())
    val uiState: StateFlow<PeersUiState> = _uiState.asStateFlow()

    init {
        loadPeers()
    }

    fun loadPeers() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            runCatching { repository.listPeers() }
                .onSuccess { peers ->
                    _uiState.value = _uiState.value.copy(
                        peers = peers.sortedWith(
                            compareByDescending<FfiPeer> { it.isOnline }
                                .thenBy { it.displayName ?: it.peerId }
                        ),
                        isLoading = false,
                    )
                }
                .onFailure { e ->
                    _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
                }
        }
    }

    fun showAddPeerDialog() {
        _uiState.value = _uiState.value.copy(showAddDialog = true)
    }

    fun hideAddPeerDialog() {
        _uiState.value = _uiState.value.copy(showAddDialog = false)
    }

    fun addPeer(peerId: String, displayName: String?, signingKeyHex: String) {
        viewModelScope.launch {
            runCatching { repository.addPeer(peerId, displayName?.ifBlank { null }, signingKeyHex) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(showAddDialog = false)
                    loadPeers()
                }
                .onFailure { e ->
                    _uiState.value = _uiState.value.copy(error = e.message)
                }
        }
    }

    fun clearError() {
        _uiState.value = _uiState.value.copy(error = null)
    }
}

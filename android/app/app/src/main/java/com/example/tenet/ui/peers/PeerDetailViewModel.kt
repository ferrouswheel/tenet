package com.example.tenet.ui.peers

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiFriendRequest
import com.example.tenet.uniffi.FfiPeer
import com.example.tenet.uniffi.FfiProfile
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class PeerDetailUiState(
    val peer: FfiPeer? = null,
    val profile: FfiProfile? = null,
    val pendingOutgoingRequest: FfiFriendRequest? = null,
    val isLoading: Boolean = false,
    val error: String? = null,
    val actionSuccess: String? = null,
)

@HiltViewModel
class PeerDetailViewModel @Inject constructor(
    savedStateHandle: SavedStateHandle,
    private val repository: TenetRepository,
) : ViewModel() {

    private val peerId: String = checkNotNull(savedStateHandle["peerId"])

    private val _uiState = MutableStateFlow(PeerDetailUiState())
    val uiState: StateFlow<PeerDetailUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            runCatching {
                val peer = repository.getPeer(peerId)
                val profile = repository.getPeerProfile(peerId)
                val requests = repository.listFriendRequests()
                val pending = requests.firstOrNull {
                    it.direction == "outgoing" &&
                        it.toPeerId == peerId &&
                        it.status == "pending"
                }
                Triple(peer, profile, pending)
            }.onSuccess { (peer, profile, pending) ->
                _uiState.value = _uiState.value.copy(
                    peer = peer,
                    profile = profile,
                    pendingOutgoingRequest = pending,
                    isLoading = false,
                )
            }.onFailure { e ->
                _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
            }
        }
    }

    fun block() {
        viewModelScope.launch {
            runCatching { repository.blockPeer(peerId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Peer blocked")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun unblock() {
        viewModelScope.launch {
            runCatching { repository.unblockPeer(peerId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Peer unblocked")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun mute() {
        viewModelScope.launch {
            runCatching { repository.mutePeer(peerId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Peer muted")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun unmute() {
        viewModelScope.launch {
            runCatching { repository.unmutePeer(peerId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Peer unmuted")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun sendFriendRequest(message: String?) {
        viewModelScope.launch {
            runCatching { repository.sendFriendRequest(peerId, message?.ifBlank { null }) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Friend request sent")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun remove() {
        viewModelScope.launch {
            runCatching { repository.removePeer(peerId) }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun clearStatus() {
        _uiState.value = _uiState.value.copy(error = null, actionSuccess = null)
    }
}

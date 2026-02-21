package com.example.tenet.ui.friends

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiFriendRequest
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class FriendsUiState(
    val incoming: List<FfiFriendRequest> = emptyList(),
    val outgoing: List<FfiFriendRequest> = emptyList(),
    val isLoading: Boolean = false,
    val error: String? = null,
    val actionSuccess: String? = null,
)

@HiltViewModel
class FriendsViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(FriendsUiState())
    val uiState: StateFlow<FriendsUiState> = _uiState.asStateFlow()

    init {
        loadRequests()
    }

    fun loadRequests() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            runCatching { repository.listFriendRequests() }
                .onSuccess { all ->
                    _uiState.value = _uiState.value.copy(
                        incoming = all.filter { it.direction == "incoming" && it.status == "pending" },
                        outgoing = all.filter { it.direction == "outgoing" },
                        isLoading = false,
                    )
                }
                .onFailure { e ->
                    _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
                }
        }
    }

    fun accept(requestId: Long) {
        viewModelScope.launch {
            runCatching { repository.acceptFriendRequest(requestId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Friend request accepted")
                    loadRequests()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun ignore(requestId: Long) {
        viewModelScope.launch {
            runCatching { repository.ignoreFriendRequest(requestId) }
                .onSuccess { loadRequests() }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun block(requestId: Long) {
        viewModelScope.launch {
            runCatching { repository.blockFriendRequest(requestId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "User blocked")
                    loadRequests()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun clearStatus() {
        _uiState.value = _uiState.value.copy(error = null, actionSuccess = null)
    }
}

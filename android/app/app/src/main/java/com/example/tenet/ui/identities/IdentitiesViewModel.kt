package com.example.tenet.ui.identities

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiIdentity
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class IdentitiesUiState(
    val identities: List<FfiIdentity> = emptyList(),
    val isLoading: Boolean = false,
    val isSwitching: Boolean = false,
    val error: String? = null,
    /** True after a successful identity switch — the UI should navigate to Timeline. */
    val switched: Boolean = false,
    /** True while the "Add Identity" dialog is open. */
    val showAddDialog: Boolean = false,
    val addRelayUrl: String = "",
    val isCreating: Boolean = false,
)

@HiltViewModel
class IdentitiesViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(IdentitiesUiState())
    val uiState: StateFlow<IdentitiesUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            try {
                val list = repository.listIdentities()
                _uiState.value = _uiState.value.copy(identities = list, isLoading = false)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(
                    isLoading = false,
                    error = e.message ?: "Failed to load identities",
                )
            }
        }
    }

    fun switchIdentity(identity: FfiIdentity) {
        if (identity.isDefault) return
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSwitching = true, error = null)
            try {
                repository.switchIdentity(identity)
                _uiState.value = _uiState.value.copy(isSwitching = false, switched = true)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(
                    isSwitching = false,
                    error = e.message ?: "Failed to switch identity",
                )
            }
        }
    }

    fun showAddDialog() {
        val currentRelay = try { repository.relayUrl() } catch (_: Exception) { "" }
        _uiState.value = _uiState.value.copy(
            showAddDialog = true,
            addRelayUrl = currentRelay,
        )
    }

    fun dismissAddDialog() {
        _uiState.value = _uiState.value.copy(showAddDialog = false, addRelayUrl = "")
    }

    fun onAddRelayUrlChange(url: String) {
        _uiState.value = _uiState.value.copy(addRelayUrl = url)
    }

    fun createIdentity() {
        val url = _uiState.value.addRelayUrl.trim()
        if (url.isBlank()) return
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isCreating = true, error = null)
            try {
                repository.createIdentity(url)
                val list = repository.listIdentities()
                _uiState.value = _uiState.value.copy(
                    identities = list,
                    isCreating = false,
                    showAddDialog = false,
                    addRelayUrl = "",
                )
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(
                    isCreating = false,
                    error = e.message ?: "Failed to create identity",
                )
            }
        }
    }

    fun clearError() {
        _uiState.value = _uiState.value.copy(error = null)
    }
}

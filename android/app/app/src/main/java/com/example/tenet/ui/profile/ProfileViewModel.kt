package com.example.tenet.ui.profile

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiProfile
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class ProfileUiState(
    val profile: FfiProfile? = null,
    val myPeerId: String = "",
    val isLoading: Boolean = false,
    val isSaving: Boolean = false,
    val isEditing: Boolean = false,
    val error: String? = null,
    val saveSuccess: Boolean = false,
)

@HiltViewModel
class ProfileViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(ProfileUiState())
    val uiState: StateFlow<ProfileUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            runCatching {
                val myId = repository.myPeerId()
                val profile = repository.getOwnProfile()
                Pair(myId, profile)
            }.onSuccess { (myId, profile) ->
                _uiState.value = _uiState.value.copy(
                    myPeerId = myId,
                    profile = profile,
                    isLoading = false,
                )
            }.onFailure { e ->
                _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
            }
        }
    }

    fun startEditing() {
        _uiState.value = _uiState.value.copy(isEditing = true, saveSuccess = false)
    }

    fun cancelEditing() {
        _uiState.value = _uiState.value.copy(isEditing = false)
    }

    fun saveProfile(displayName: String?, bio: String?) {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSaving = true, error = null)
            runCatching {
                repository.updateOwnProfile(
                    displayName = displayName?.ifBlank { null },
                    bio = bio?.ifBlank { null },
                    avatarHash = null, // avatar upload handled separately via attachment API
                )
            }.onSuccess {
                _uiState.value = _uiState.value.copy(isSaving = false, isEditing = false, saveSuccess = true)
                load()
            }.onFailure { e ->
                _uiState.value = _uiState.value.copy(isSaving = false, error = e.message)
            }
        }
    }

    fun clearStatus() {
        _uiState.value = _uiState.value.copy(error = null, saveSuccess = false)
    }
}

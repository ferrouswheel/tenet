package com.example.tenet.ui.compose

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class ComposeUiState(
    val initialBody: String = "",
    val isSending: Boolean = false,
    val error: String? = null,
    val done: Boolean = false,
)

/**
 * ViewModel for the Compose screen.
 *
 * Reads an optional `sharedText` navigation argument from [SavedStateHandle].
 * This argument is set by [com.example.tenet.ui.TenetNavHost] when the app is
 * opened via a share-to intent, pre-populating the message body.
 */
@HiltViewModel
class ComposeViewModel @Inject constructor(
    private val repository: TenetRepository,
    savedStateHandle: SavedStateHandle,
) : ViewModel() {

    private val _uiState = MutableStateFlow(
        ComposeUiState(
            // sharedText is URL-decoded by Navigation Compose before being placed
            // in SavedStateHandle, so no extra decoding is required here.
            initialBody = savedStateHandle.get<String>("sharedText") ?: "",
        )
    )
    val uiState: StateFlow<ComposeUiState> = _uiState.asStateFlow()

    fun send(body: String) {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSending = true, error = null)
            try {
                repository.sendPublic(body)
                _uiState.value = _uiState.value.copy(done = true)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(
                    isSending = false,
                    error = e.message ?: "Send failed",
                )
            }
        }
    }
}

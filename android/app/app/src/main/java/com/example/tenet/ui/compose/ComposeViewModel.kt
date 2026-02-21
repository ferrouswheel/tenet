package com.example.tenet.ui.compose

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
    val isSending: Boolean = false,
    val error: String? = null,
    val done: Boolean = false,
)

@HiltViewModel
class ComposeViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(ComposeUiState())
    val uiState: StateFlow<ComposeUiState> = _uiState.asStateFlow()

    fun send(body: String) {
        viewModelScope.launch {
            _uiState.value = ComposeUiState(isSending = true)
            try {
                repository.sendPublic(body)
                _uiState.value = ComposeUiState(done = true)
            } catch (e: Exception) {
                _uiState.value = ComposeUiState(error = e.message ?: "Send failed")
            }
        }
    }
}

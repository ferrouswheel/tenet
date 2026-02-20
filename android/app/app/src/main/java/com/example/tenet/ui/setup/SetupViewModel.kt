package com.example.tenet.ui.setup

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class SetupUiState(
    val isLoading: Boolean = false,
    val error: String? = null,
    val done: Boolean = false,
)

@HiltViewModel
class SetupViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(SetupUiState())
    val uiState: StateFlow<SetupUiState> = _uiState.asStateFlow()

    fun initialize(relayUrl: String) {
        viewModelScope.launch {
            _uiState.value = SetupUiState(isLoading = true)
            try {
                repository.initialize(relayUrl)
                _uiState.value = SetupUiState(done = true)
            } catch (e: Exception) {
                _uiState.value = SetupUiState(error = e.message ?: "Initialization failed")
            }
        }
    }
}

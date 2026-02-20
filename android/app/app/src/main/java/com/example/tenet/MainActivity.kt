package com.example.tenet

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import androidx.navigation.compose.rememberNavController
import com.example.tenet.theme.TenetTheme
import com.example.tenet.ui.TenetNavHost
import dagger.hilt.android.AndroidEntryPoint

/**
 * Single-activity entry point.  Navigation is handled entirely by
 * [TenetNavHost] via Navigation Compose.
 */
@AndroidEntryPoint
class MainActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            TenetTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background
                ) {
                    val navController = rememberNavController()
                    TenetNavHost(navController = navController)
                }
            }
        }
    }
}

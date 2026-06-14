plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.dumbcoder.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.dumbcoder.android"
        minSdk = 26 // ML Kit GenAI Prompt API requires API 26+
        targetSdk = 36
        versionCode = 1
        versionName = "0.0.1"
        // The Rust core (.so) is built per-ABI by cargo-ndk into src/main/jniLibs/.
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    buildTypes {
        release { isMinifyEnabled = false }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    // On-device Gemini Nano / Gemma 4 via AICore (beta — confirm version).
    implementation("com.google.mlkit:genai-prompt:1.0.0-beta2")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
}

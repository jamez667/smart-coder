plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.smartcoder.remote"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.smartcoder.remote"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        // The Rust core (.so) ships prebuilt in jniLibs/arm64-v8a for on-device mode.
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
    buildFeatures { compose = true }
    // Page-align native libs (uncompressed, extracted) so they satisfy the 16 KB
    // page-size check on newer devices instead of triggering the compat warning.
    packaging { jniLibs { useLegacyPackaging = false } }
}

// Force a single ML Kit GenAI version so beta1 and beta2 don't clash on the classpath.
// Also force graphics-path 1.0.2 — 1.0.1's native .so isn't 16 KB-page aligned (fails
// the 16 KB compatibility check on newer devices).
configurations.all {
    resolutionStrategy {
        force("com.google.mlkit:genai-prompt:1.0.0-beta1")
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.09.03")
    implementation(composeBom)
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")
    implementation("androidx.security:security-crypto:1.1.0-alpha06")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
    // On-device Gemini Nano via ML Kit GenAI (AICore). Requires a supported device.
    // Pinned to beta1 to avoid a beta1/beta2 class clash on the compile classpath.
    implementation("com.google.mlkit:genai-prompt:1.0.0-beta1")
}

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "ai.weftos.phonenode"
    compileSdk = 35

    defaultConfig {
        applicationId = "ai.weftos.phonenode"
        minSdk = 29
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0-p0"
        // The Rust core is built for the S24 Ultra (the only reference device, design Q2).
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
        debug {
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { compose = true }

    // `scripts/build.sh android` writes the cargo-ndk .so files and the UniFFI Kotlin here.
    sourceSets["main"].kotlin.srcDir("src/main/generated-uniffi")
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    debugImplementation("androidx.compose.ui:ui-tooling")

    // UniFFI's generated Kotlin loads the Rust library through JNA.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
}

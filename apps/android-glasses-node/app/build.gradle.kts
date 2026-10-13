plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.weftos.glasses"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.weftos.glasses"
        // The glasses run Android 11 (API 30), MT6761, arm64.
        minSdk = 30
        // 34 matches the working mentramic capture app: the shell-started microphone
        // foreground service exemption is what keeps the mic alive with the display off.
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0-p0"
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    // `scripts/build.sh glasses` writes the cargo-ndk .so files and the UniFFI Kotlin here.
    sourceSets["main"].kotlin.srcDir("src/main/generated-uniffi")
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")

    testOptions { unitTests.isReturnDefaultValues = true }
}

dependencies {
    // UniFFI's generated Kotlin loads the Rust library through JNA.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    testImplementation("junit:junit:4.13.2")
}

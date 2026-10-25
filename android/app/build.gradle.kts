plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "ru.skreenshare.proektor"
    compileSdk = 34

    defaultConfig {
        applicationId = "ru.skreenshare.proektor"
        minSdk = 21
        targetSdk = 34
        versionCode = 5
        versionName = "0.5.0"
    }

    // Two APKs from the same code, signed with the same key (either one installs over the other):
    //  standard - Android 5.0+ (API 21), targets 34;
    //  compat   - for old/odd firmware (some projectors, TV boxes): Android 4.4+ (API 19), targets 28 (Android 9),
    //             so the system applies none of the newer restrictions to it.
    flavorDimensions += "build"
    productFlavors {
        create("standard") {
            dimension = "build"
        }
        create("compat") {
            dimension = "build"
            minSdk = 19
            targetSdk = 28
            versionNameSuffix = "-compat"
        }
    }

    signingConfigs {
        // A fixed, non-secret key: every build has the same signature, so a new APK installs over the old one.
        create("fixed") {
            storeFile = file("../release.keystore")
            storePassword = "skreenshare"
            keyAlias = "skreenshare"
            keyPassword = "skreenshare"
            // All schemes on: v1 (JAR) for old installers and file managers, v2/v3 for new ones.
            enableV1Signing = true
            enableV2Signing = true
            enableV3Signing = true
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("fixed")
        }
        debug {
            signingConfig = signingConfigs.getByName("fixed")
        }
    }

    packaging {
        resources.excludes += setOf("META-INF/*.version", "META-INF/*.kotlin_module", "kotlin/**", "DebugProbesKt.bin")
    }
    dependenciesInfo {
        includeInApk = false // no extra signing-block blob that some old installers dislike
        includeInBundle = false
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
}

dependencies {
    testImplementation("junit:junit:4.13.2")
}

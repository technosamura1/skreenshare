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
        versionCode = 1
        versionName = "0.1.0"
    }

    signingConfigs {
        // A fixed, non-secret key: every build has the same signature, so a new APK installs over the old one.
        create("fixed") {
            storeFile = file("../release.keystore")
            storePassword = "skreenshare"
            keyAlias = "skreenshare"
            keyPassword = "skreenshare"
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

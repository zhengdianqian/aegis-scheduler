// Copyright (C) 2026 默默的阿文
plugins {
    id("com.android.application")
}

android {
    namespace = "com.aegis.scheduler"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.aegis.scheduler"
        minSdk = 24
        targetSdk = 36
        versionCode = 7
        versionName = "2.2"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

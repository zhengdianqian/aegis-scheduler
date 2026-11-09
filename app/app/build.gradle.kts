import java.io.StringReader
import java.util.Properties

// Copyright (C) 2026 默默的阿文
plugins {
    id("com.android.application")
}

// 正式签名配置位于 app/keystore/，密钥库可用相对或绝对路径指定；debug 构建不受影响。
val keystorePropsFile = rootProject.file("keystore/keystore.properties")
val keystoreProps = Properties()
val releaseSigningReady = keystorePropsFile.exists()
if (releaseSigningReady) {
    // 兼容带 UTF-8 BOM 的 properties 文件，避免首个键名被 BOM 污染。
    keystoreProps.load(StringReader(keystorePropsFile.readText().removePrefix("\uFEFF")))
}

android {
    namespace = "com.aegis.scheduler"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.aegis.scheduler"
        minSdk = 24
        targetSdk = 36
        versionCode = 17
        versionName = "3.4.0"
    }

    signingConfigs {
        if (releaseSigningReady) {
            create("release") {
                storeFile = keystorePropsFile.parentFile.resolve(keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
                enableV1Signing = true
                enableV2Signing = true
                enableV3Signing = true
                enableV4Signing = false
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            if (releaseSigningReady) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

// 缺少正式签名时，构建 release 直接失败；绝不产出未签名包或回退到 debug 签名。
gradle.taskGraph.whenReady {
    if (hasTask(":app:packageRelease") && !releaseSigningReady) {
        throw GradleException(
            "release 签名配置缺失：未找到 ${keystorePropsFile.absolutePath}。" +
                "请确认 app/keystore/keystore.properties 存在并指向正式签名密钥库。"
        )
    }
}

import java.text.SimpleDateFormat
import java.util.Date

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.xrproxy.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.xrproxy.app"
        minSdk = 29
        targetSdk = 34
        versionCode = 1
        // `git describe --always --dirty` даёт короткий хеш HEAD плюс "-dirty",
        // если в рабочем дереве есть не-закоммиченные правки. buildStamp (HHmm)
        // делает versionName уникальным между сборками одного и того же коммита
        // — без этого при dirty-разработке экран "v0.1.0-<hash>-dirty" выглядит
        // одинаково на каждом новом APK, и непонятно, какая сборка установлена.
        val gitDescribe = providers.exec {
            commandLine("git", "describe", "--always", "--dirty", "--abbrev=7")
        }.standardOutput.asText.get().trim()
        val buildStamp = SimpleDateFormat("HHmm").format(Date())
        versionName = "0.1.0-$gitDescribe-$buildStamp"

        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Подпись debug-ключом — это НЕ production-release. Нужна, чтобы
            // локальная release-сборка (для проверки скорости/оптимизаций)
            // была установимой без настройки отдельного keystore. Для
            // публикации в Play Store потом настраивается отдельный
            // signingConfig с production-ключом.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.4")
    implementation("androidx.activity:activity-compose:1.9.1")

    // Compose
    implementation(platform("androidx.compose:compose-bom:2024.06.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")

    // ViewModel
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.4")

    // Security (encrypted preferences)
    implementation("androidx.security:security-crypto:1.1.0-alpha06")
}

// Task to build Rust native libraries before Android build.
tasks.register<Exec>("buildRustRelease") {
    workingDir = file("${project.rootDir}/..")
    environment("ANDROID_NDK_HOME", "${android.ndkDirectory}")
    commandLine(
        "cargo", "ndk",
        "-t", "aarch64-linux-android",
        "-t", "x86_64-linux-android",
        "-o", "${projectDir}/src/main/jniLibs",
        "build", "-p", "xr-android-jni", "--release"
    )
}

tasks.register<Exec>("buildRustDebug") {
    workingDir = file("${project.rootDir}/..")
    environment("ANDROID_NDK_HOME", "${android.ndkDirectory}")
    commandLine(
        "cargo", "ndk",
        "-t", "aarch64-linux-android",
        "-o", "${projectDir}/src/main/jniLibs",
        "build", "-p", "xr-android-jni"
    )
}

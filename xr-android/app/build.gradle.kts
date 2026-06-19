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
        // versionCode гейтит самообновление (LLD-12): приложение предлагает
        // апдейт только если version_code манифеста строго больше
        // установленного. Бампать на каждый релиз; переопределяется проперти
        // `xrVersionCode` (или env ORG_GRADLE_PROJECT_xrVersionCode) без правки
        // файла. Дефолт 1.
        versionCode = (project.findProperty("xrVersionCode") as String?)?.toIntOrNull() ?: 1
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

        // Pinned ed25519 release public key for APK self-update (LLD-12 §2.2).
        // The PRIVATE half lives offline with the owner; only the PUBLIC half
        // is compiled in here, via the gradle property `xrReleasePublicKey`
        // (set in gradle.properties / ~/.gradle/gradle.properties or
        // `-PxrReleasePublicKey=...`). Empty ⇒ self-update is disabled (the
        // check returns `no_release_key` and the UI stays silent). The public
        // key is NOT a secret, so committing it to a properties file is fine;
        // signing with the matching private key is what gates an update.
        val releasePublicKey = (project.findProperty("xrReleasePublicKey") as String?) ?: ""
        buildConfigField("String", "RELEASE_PUBLIC_KEY", "\"$releasePublicKey\"")
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
        // Required by AGP 8 to emit custom buildConfigField entries
        // (RELEASE_PUBLIC_KEY above).
        buildConfig = true
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

    // Google Code Scanner (system-UI QR scanner, no CAMERA permission needed)
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")

    // Явный апгрейд androidx.fragment — play-services-code-scanner транзитивно
    // тащит старый fragment, а lint требует >= 1.3.0 для registerForActivityResult.
    implementation("androidx.fragment:fragment-ktx:1.8.3")
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

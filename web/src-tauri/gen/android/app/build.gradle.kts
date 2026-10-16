import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

// The shell crate's own version, which a release bumps: tauri.properties
// carries Android's 1.0 default, and a build labelled 1.0 would be a lie.
val shellVersion: String = Regex("""(?m)^version = "([^"]+)"""")
    .find(file("../../../Cargo.toml").readText())
    ?.groupValues
    ?.get(1) ?: "1.0"
val shellVersionCode: Int = shellVersion.split('.').let { parts ->
    (parts.getOrNull(0)?.toIntOrNull() ?: 1) * 1_000_000 +
        (parts.getOrNull(1)?.toIntOrNull() ?: 0) * 1_000 +
        (parts.getOrNull(2)?.toIntOrNull() ?: 0)
}

// The release keystore, read through the gitignored key.properties the
// README documents. Absent, a release build is unsigned rather than wrongly
// signed, and `just android-release` is the gate that refuses it.
val keyProperties = Properties().apply {
    val propFile = file("key.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}
val releaseKeystore = keyProperties.getProperty("storeFile")

android {
    compileSdk = 37
    namespace = "dev.vedhavyas.nazar"
    defaultConfig {
        // The app reaches the server at http://nazar.hub over the WireGuard
        // mesh, and Android has blocked cleartext by default since 9.
        manifestPlaceholders["usesCleartextTraffic"] = "true"
        applicationId = "dev.vedhavyas.nazar"
        minSdk = 26
        targetSdk = 37
        versionCode = shellVersionCode
        versionName = shellVersion
    }
    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = keyProperties.getProperty("storePassword")
                keyAlias = keyProperties.getProperty("keyAlias")
                keyPassword = keyProperties.getProperty("keyPassword")
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            optimization {
               enable = true
            }
            proguardFiles(
                *fileTree(".") {
                  include("**/*.pro")
                  exclude("build/**")
                }.files.toTypedArray()
            )
            if (releaseKeystore != null) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    buildFeatures {
        buildConfig = true
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = file("tauri.build.gradle.kts"))

import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

val branding = Properties().apply {
    rootProject.file("branding.properties").reader(Charsets.UTF_8).use { load(it) }
}
val displayName = branding.getProperty("APP_DISPLAY_NAME").trim()
val ownerLogin = branding.getProperty("OWNER_GITHUB_LOGIN").trim()
val releaseRepo = branding.getProperty("RELEASE_REPO").trim()
val supportUrl = branding.getProperty("SUPPORT_URL").trim()
val productionId = branding.getProperty("APPLICATION_ID").trim()
val production = providers.gradleProperty("tgwsProduction").map(String::toBoolean).getOrElse(false)
val releasePreview = providers.gradleProperty("tgwsReleasePreview").map(String::toBoolean).getOrElse(false)
val versionNameDefault = "1.0.0"
val versionCodeDefault = 1250
val releaseVersionCode = providers.gradleProperty("tgwsReleaseVersionCode")
    .map(String::toInt).getOrElse(versionCodeDefault)
val releaseVersionName = providers.gradleProperty("tgwsReleaseVersionName").getOrElse(versionNameDefault)
val playAbiVersionCodes = providers.gradleProperty("tgwsPlayAbiVersionCodes")
    .map(String::toBoolean).getOrElse(false)
val developmentId = "com.amurcanov.tgwsproxy"

require(!production || !releasePreview) { "Choose either production or unsigned release preview" }

if (production) {
    require(releaseVersionCode in 1..2_100_000_000) {
        "Set a positive tgwsReleaseVersionCode for production"
    }
    require(releaseVersionName.matches(Regex("[0-9]+\\.[0-9]+\\.[0-9]+(?:-[a-zA-Z0-9.-]+)?"))) {
        "Set tgwsReleaseVersionName, for example 1.0.0"
    }
    require(ownerLogin.matches(Regex("[A-Za-z0-9][A-Za-z0-9-]{0,38}"))) { "Set OWNER_GITHUB_LOGIN in branding.properties" }
    require(releaseRepo.startsWith("$ownerLogin/") && releaseRepo.matches(Regex("[A-Za-z0-9-]+/[A-Za-z0-9_.-]+"))) {
        "Set RELEASE_REPO to the verified owner/repository in branding.properties"
    }
    require(productionId.matches(Regex("[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)+")) &&
        !productionId.startsWith(developmentId) &&
        !productionId.endsWith(".rustdev") &&
        !productionId.endsWith(".debug") &&
        !productionId.contains("releasepreview")) {
        "Set a final production APPLICATION_ID in branding.properties (not releasepreview/rustdev/upstream)"
    }
}

fun buildStringLiteral(value: String): String = "\"${value.replace("\\", "\\\\").replace("\"", "\\\"")}\""

android {
    namespace = "com.amurcanov.tgwsproxy"
    compileSdk = 35

    defaultConfig {
        applicationId = if (production) productionId else developmentId
        targetSdk = 35
        versionCode = if (production) releaseVersionCode else versionCodeDefault
        versionName = if (production) releaseVersionName else versionNameDefault
        resValue("string", "app_name", displayName)
        buildConfigField("String", "OWNER_GITHUB_LOGIN", buildStringLiteral(if (production) ownerLogin else ""))
        buildConfigField("String", "RELEASE_REPO", buildStringLiteral(if (production) releaseRepo else ""))
        buildConfigField("String", "SUPPORT_URL", buildStringLiteral(if (production) supportUrl else ""))

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        vectorDrawables {
            useSupportLibrary = true
        }
    }

    flavorDimensions.add("arch")
    val baseVersionCode = if (production) releaseVersionCode else versionCodeDefault
    productFlavors {
        create("arm32") {
            dimension = "arch"
            minSdk = 21
            versionCode = if (playAbiVersionCodes) baseVersionCode * 10 + 2 else baseVersionCode
            ndk {
                abiFilters.clear()
                abiFilters.add("armeabi-v7a")
            }
        }
        create("arm64") {
            dimension = "arch"
            // Linked against NDK API 21 with 16 KB ELF page alignment.
            minSdk = 21
            versionCode = if (playAbiVersionCodes) baseVersionCode * 10 + 3 else baseVersionCode
            ndk {
                abiFilters.clear()
                abiFilters.add("arm64-v8a")
            }
        }
        create("universal") {
            dimension = "arch"
            // Both arm64-v8a and armeabi-v7a libraries are linked against NDK API 21.
            minSdk = 21
            versionCode = if (playAbiVersionCodes) baseVersionCode * 10 + 1 else baseVersionCode
            ndk {
                abiFilters.clear()
                abiFilters.addAll(listOf("arm64-v8a", "armeabi-v7a"))
            }
        }
    }

    val localProperties = Properties()
    val localPropertiesFile = rootProject.file("local.properties")
    if (localPropertiesFile.exists()) {
        localProperties.load(localPropertiesFile.inputStream())
    }

    fun signingValue(propKey: String, envKey: String): String? =
        localProperties.getProperty(propKey)?.trim()?.takeIf { it.isNotEmpty() }
            ?: System.getenv(envKey)?.trim()?.takeIf { it.isNotEmpty() }

    val configuredKeyFile = signingValue("KEYSTORE_FILE", "TGWS_KEYSTORE_FILE")
    val resolvedKeystoreFile = configuredKeyFile?.let { path ->
        if (path.startsWith("..")) file(rootDir.resolve(path.substring(3))) else file(path)
    }
    val configuredStorePassword = signingValue("KEYSTORE_PASSWORD", "TGWS_KEYSTORE_PASSWORD")
    val configuredKeyAlias = signingValue("KEY_ALIAS", "TGWS_KEY_ALIAS")
    val configuredKeyPassword = signingValue("KEY_PASSWORD", "TGWS_KEY_PASSWORD")

    signingConfigs {
        create("release") {
            if (resolvedKeystoreFile != null) {
                if (resolvedKeystoreFile.isFile) {
                    storeFile = resolvedKeystoreFile
                    storePassword = configuredStorePassword
                    keyAlias = configuredKeyAlias
                    keyPassword = configuredKeyPassword
                } else {
                    println("WARNING: Keystore file not found: $configuredKeyFile (resolved: ${resolvedKeystoreFile.absolutePath})")
                }
            }
            enableV1Signing = true
            enableV2Signing = true
            enableV3Signing = true
        }
    }

    buildTypes {
        getByName("debug") {
            // Side-by-side with the installed release app. Do not replace it.
            applicationIdSuffix = ".rustdev"
        }
        getByName("release") {
            if (releasePreview) {
                applicationIdSuffix = ".releasepreview"
                resValue("string", "app_name", "$displayName Review")
            }
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            if (production) {
                require(
                    resolvedKeystoreFile?.isFile == true &&
                        !configuredStorePassword.isNullOrBlank() &&
                        !configuredKeyAlias.isNullOrBlank() &&
                        !configuredKeyPassword.isNullOrBlank()
                ) {
                    "Production release requires a private keystore via local.properties (KEYSTORE_FILE, KEYSTORE_PASSWORD, KEY_ALIAS, KEY_PASSWORD) or env vars (TGWS_KEYSTORE_FILE, TGWS_KEYSTORE_PASSWORD, TGWS_KEY_ALIAS, TGWS_KEY_PASSWORD)."
                }
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        compose = true
        buildConfig = true
        resValues = true
    }
    // composeOptions removed — AGP 9.x handles Compose compiler internally
    packaging {
        jniLibs.keepDebugSymbols += "**/libtgwsproxy.so"
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }
    sourceSets {
        getByName("main") {
            jniLibs.setSrcDirs(listOf(providers.gradleProperty("tgwsNativeLibsDir")
                .getOrElse(rootProject.layout.buildDirectory.dir("generated/tgwsNative").get().asFile.absolutePath)))
        }
    }
}

androidComponents {
    beforeVariants { variant ->
        variant.enable = if (production || releasePreview) variant.buildType == "release" else variant.buildType == "debug"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")

    // DataStore for persistent settings
    implementation("androidx.datastore:datastore-preferences:1.1.1")

    // JNA for easy C-shared library calls
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.appcompat:appcompat:1.6.1")
    testImplementation("junit:junit:4.13.2")
}

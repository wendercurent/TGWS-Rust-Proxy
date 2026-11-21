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
val versionCodeDefault = 124
val releaseVersionCode = providers.gradleProperty("tgwsReleaseVersionCode")
    .map(String::toInt).getOrElse(versionCodeDefault)
val releaseVersionName = providers.gradleProperty("tgwsReleaseVersionName").getOrElse(versionNameDefault)
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
        !productionId.startsWith("$developmentId") && !productionId.endsWith(".rustdev")) {
        "Set a distinct APPLICATION_ID under a namespace you control"
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
    productFlavors {
        create("arm32") {
            dimension = "arch"
            minSdk = 21
            ndk {
                abiFilters.clear()
                abiFilters.add("armeabi-v7a")
            }
        }
        create("arm64") {
            dimension = "arch"
            minSdk = 24
            ndk {
                abiFilters.clear()
                abiFilters.add("arm64-v8a")
            }
        }
        create("universal") {
            dimension = "arch"
            // The ARM64 library is linked against NDK API 24.
            minSdk = 24
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

    signingConfigs {
        create("release") {
            val keyFile = localProperties.getProperty("KEYSTORE_FILE")
            if (keyFile != null) {
                // Резолвим путь: если начинается с "..", берём от корня проекта
                val resolvedFile = if (keyFile.startsWith("..")) {
                    // ../release.keystore -> корень проекта / release.keystore
                    file(rootDir.resolve(keyFile.substring(3)))
                } else {
                    file(keyFile)
                }
                if (resolvedFile.exists()) {
                    storeFile = resolvedFile
                    storePassword = localProperties.getProperty("KEYSTORE_PASSWORD")
                    keyAlias = localProperties.getProperty("KEY_ALIAS")
                    keyPassword = localProperties.getProperty("KEY_PASSWORD")
                } else {
                    println("WARNING: Keystore file not found: $keyFile (resolved: ${resolvedFile.absolutePath})")
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
            val keyFile = localProperties.getProperty("KEYSTORE_FILE")
            val resolvedFile = if (keyFile != null && keyFile.startsWith("..")) {
                file(rootDir.resolve(keyFile.substring(3)))
            } else if (keyFile != null) {
                file(keyFile)
            } else null
            
            if (production) {
                require(resolvedFile?.isFile == true && listOf("KEYSTORE_PASSWORD", "KEY_ALIAS", "KEY_PASSWORD")
                    .all { !localProperties.getProperty(it).isNullOrBlank() }) {
                    "Production requires a private release keystore and all signing properties"
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
    debugImplementation("com.squareup.leakcanary:leakcanary-android:2.14")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.appcompat:appcompat:1.6.1")
    testImplementation("junit:junit:4.13.2")
}

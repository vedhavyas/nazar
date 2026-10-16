package dev.vedhavyas.nazar

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import androidx.core.content.FileProvider
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import org.json.JSONObject

private const val USER_AGENT = "nazar-android"
private const val CONNECT_TIMEOUT_MS = 15_000
private const val READ_TIMEOUT_MS = 60_000

@InvokeArg
// Field for field with `CheckArgs` in the shell's `android` module.
class CheckArgs {
  lateinit var version: String
  lateinit var endpoint: String
}

@InvokeArg
// Field for field with `InstallArgs` in the shell's `android` module.
class InstallArgs {
  lateinit var version: String
  lateinit var url: String
}

/**
 * A release version as three integers, or `null` for anything else - a
 * prerelease suffix, a fourth segment, junk. Every segment must parse: a
 * compare that dropped the ones that did not would read `x.1.0.116` as
 * `1.0.116` and offer it.
 */
internal fun versionParts(version: String): List<Int>? {
  val segments = version.trim().trimStart('v').split('.')
  if (segments.size != 3) {
    return null
  }
  return segments.map { it.toIntOrNull() ?: return null }
}

/** Strictly newer, and only when both sides parse; anything else is no offer. */
internal fun isNewerVersion(candidate: String, current: String): Boolean {
  val next = versionParts(candidate) ?: return false
  val held = versionParts(current) ?: return false
  for (index in 0..2) {
    if (next[index] != held[index]) {
      return next[index] > held[index]
    }
  }
  return false
}

/** The release a manifest names, in the shape the app's own version takes. */
internal fun normalizeVersion(version: String): String? = versionParts(version)?.joinToString(".")

/**
 * The phone's half of the shell's own update: reads the release manifest,
 * downloads the APK, checks it against this install, and hands it to the
 * system installer - which is the only install path a sideloaded app has.
 *
 * The shell's own commands bridge here, so the update check is one surface
 * on every platform; this class carries the parts that must be Android's
 * (the package manager's signer read and the installer intent).
 */
@TauriPlugin
class UpdatePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun check(invoke: Invoke) {
    val args = invoke.parseArgs(CheckArgs::class.java)
    background(invoke) {
      val manifest = fetch(args.endpoint)
      val android = manifest.optJSONObject("android")
      val version = normalizeVersion(manifest.optString("version"))
      if (android != null && version != null && isNewerVersion(version, args.version)) {
        invoke.resolve(JSObject().put("version", version).put("url", android.getString("url")))
      } else {
        invoke.resolve()
      }
    }
  }

  @Command
  fun install(invoke: Invoke) {
    val args = invoke.parseArgs(InstallArgs::class.java)
    background(invoke) {
      val apk = download(args.version, args.url)
      try {
        verify(apk, args.version)
      } catch (err: Exception) {
        // A file that failed its check must not be the one a later tap reuses.
        apk.delete()
        throw err
      }
      handToInstaller(invoke, apk)
    }
  }

  /** Commands run on the main thread, and all of this is network and disk. */
  private fun background(invoke: Invoke, work: () -> Unit) {
    Thread {
      try {
        work()
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }.start()
  }

  private fun fetch(endpoint: String): JSONObject {
    val connection = URL(endpoint).openConnection() as HttpURLConnection
    connection.connectTimeout = CONNECT_TIMEOUT_MS
    connection.readTimeout = READ_TIMEOUT_MS
    connection.setRequestProperty("User-Agent", USER_AGENT)
    connection.setRequestProperty("Accept", "application/json")
    try {
      connection.inputStream.use { stream ->
        return JSONObject(stream.readBytes().decodeToString())
      }
    } finally {
      connection.disconnect()
    }
  }

  /** The file for `version`, downloaded once: a verified one is reused. */
  private fun download(version: String, url: String): File {
    val file = File(activity.cacheDir, "nazar-$version.apk")
    // Only the release being offered is worth keeping: one that moved on
    // leaves every earlier download unused. Pruned before the reuse check
    // too, or a cached offer leaves the earlier files behind for good.
    activity.cacheDir
      .listFiles { candidate -> candidate.name.startsWith("nazar-") && candidate.name.endsWith(".apk") }
      ?.forEach { stale ->
        if (stale.name != file.name) {
          stale.delete()
        }
      }
    if (file.exists()) {
      return file
    }
    val connection = URL(url).openConnection() as HttpURLConnection
    connection.connectTimeout = CONNECT_TIMEOUT_MS
    connection.readTimeout = READ_TIMEOUT_MS
    connection.setRequestProperty("User-Agent", USER_AGENT)
    try {
      connection.inputStream.use { stream ->
        file.outputStream().use { stream.copyTo(it) }
      }
    } catch (err: Exception) {
      file.delete()
      throw err
    } finally {
      connection.disconnect()
    }
    return file
  }

  /**
   * The download must be this app and the version the manifest named,
   * carrying this install's own signer - the identity the system installer
   * checks anyway, so a mismatch is refused here with the reason rather than
   * as an install failure later. A debug-signed install therefore refuses the
   * release APK instead of trying an upgrade the installer would reject.
   */
  private fun verify(apk: File, version: String) {
    val manager = activity.packageManager
    val flags = signerFlags()
    val archive =
      manager.getPackageArchiveInfo(apk.absolutePath, flags)
        ?: throw IllegalStateException("the downloaded file is not an app package")
    if (archive.packageName != activity.packageName) {
      throw IllegalStateException("the download is the app ${archive.packageName}, not this one")
    }
    if (!isVersion(archive.versionName, version)) {
      throw IllegalStateException("the download is version ${archive.versionName}, expected $version")
    }
    val mine = digestOf(manager.getPackageInfo(activity.packageName, flags))
    val theirs = digestOf(archive)
    if (mine != theirs) {
      throw IllegalStateException("the download's signer is $theirs, this install's is $mine")
    }
  }

  /** Version equality by the same parse the offer decision runs on. */
  private fun isVersion(candidate: String?, expected: String): Boolean {
    val named = normalizeVersion(candidate ?: return false) ?: return false
    return named == normalizeVersion(expected)
  }

  private fun signerFlags(): Int =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
      PackageManager.GET_SIGNING_CERTIFICATES
    } else {
      @Suppress("DEPRECATION") PackageManager.GET_SIGNATURES
    }

  private fun digestOf(info: PackageInfo): String {
    val signer =
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
        info.signingInfo?.apkContentsSigners?.firstOrNull()
      } else {
        @Suppress("DEPRECATION") info.signatures?.firstOrNull()
      } ?: throw IllegalStateException("a signer could not be read")
    return MessageDigest.getInstance("SHA-256").digest(signer.toByteArray()).joinToString("") {
      "%02x".format(it)
    }
  }

  private fun handToInstaller(invoke: Invoke, apk: File) {
    val uri: Uri = FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", apk)
    val intent =
      Intent(Intent.ACTION_VIEW).apply {
        setDataAndType(uri, "application/vnd.android.package-archive")
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      }
    // On the UI thread, and answered from inside it: a device with nothing
    // handling the intent throws at startActivity, and that must be a
    // rejection rather than a crash on the main thread.
    activity.runOnUiThread {
      try {
        activity.startActivity(intent)
        invoke.resolve()
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }
  }
}

// Copyright (C) 2026 默默的阿文
package com.aegis.scheduler

import android.app.Activity
import android.app.AlertDialog
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.drawable.BitmapDrawable
import android.graphics.drawable.Drawable
import android.os.Bundle
import android.util.Base64
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * Aegis 控制台容器。
 *
 * 界面完全复用 WebUI 的静态产物（assets/webui），原生层只做三件事：
 * 1. 提供 su 命令通道（window.AegisShell.exec），把配置读写落到 /data/adb/tempo；
 * 2. 提供 PackageManager 的全部应用名与真实图标（window.AegisShell.apps）；
 * 3. 开屏检测一次 root 状态，未授权时给出引导。
 */
class MainActivity : Activity() {

    private lateinit var webView: WebView
    private val executor = Executors.newSingleThreadExecutor()
    private val iconCache = HashMap<String, String>()
    private var rootCheckStarted = false
    private var rootDialog: AlertDialog? = null
    private var appsCache: String? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val view = WebView(this)
        view.setBackgroundColor(0xFF071018.toInt())
        webView = view
        setContentView(view)

        with(view.settings) {
            javaScriptEnabled = true
            domStorageEnabled = true
            allowFileAccess = true
            allowContentAccess = false
            mediaPlaybackRequiresUserGesture = true
            loadWithOverviewMode = true
            useWideViewPort = true
            cacheMode = WebSettings.LOAD_DEFAULT
        }
        view.webViewClient = WebViewClient()
        view.webChromeClient = WebChromeClient()
        view.addJavascriptInterface(Bridge(), "AegisShell")
        view.loadUrl("file:///android_asset/webui/index.html")

        checkRootOnce()
    }

    override fun onResume() {
        super.onResume()
        if (this::webView.isInitialized) webView.onResume()
    }

    override fun onPause() {
        // 切到后台时暂停 WebView 的 JS 计时器，避免 APP 与 KernelSU WebUI 同时轮询。
        if (this::webView.isInitialized) webView.onPause()
        super.onPause()
    }

    @Deprecated("Deprecated in Android 13, kept for older devices")
    override fun onBackPressed() {
        if (this::webView.isInitialized && webView.canGoBack()) {
            webView.goBack()
        } else {
            @Suppress("DEPRECATION")
            super.onBackPressed()
        }
    }

    override fun onDestroy() {
        rootDialog?.dismiss()
        rootDialog = null
        executor.shutdownNow()
        if (this::webView.isInitialized) webView.destroy()
        super.onDestroy()
    }

    /* ---------- root ---------- */

    /**
     * 开屏只检测一次。授权成功会记录到本地，之后启动不再弹窗；
     * 未授权时给出一次明确的授权引导。
     */
    private fun checkRootOnce() {
        if (rootCheckStarted) return
        rootCheckStarted = true
        executor.execute {
            val rooted = hasRoot()
            getSharedPreferences(PREFS, MODE_PRIVATE).edit()
                .putBoolean(KEY_ROOT_OK, rooted)
                .apply()
            if (!rooted) {
                runOnUiThread { showRootDialog() }
            }
        }
    }

    private fun hasRoot(): Boolean {
        val attempts = listOf(
            arrayOf("su", "-c", "id"),
            arrayOf("su", "0", "sh", "-c", "id"),
            arrayOf("su", "-c", "id -u")
        )
        for (argv in attempts) {
            try {
                val process = ProcessBuilder(*argv)
                    .directory(File("/"))
                    .redirectErrorStream(true)
                    .start()
                if (!process.waitFor(8, TimeUnit.SECONDS)) {
                    process.destroyForcibly()
                    continue
                }
                val output = process.inputStream.bufferedReader().use { it.readText() }
                val text = output.trim()
                if (process.exitValue() == 0 && (text.contains("uid=0") || text == "0")) {
                    return true
                }
            } catch (ignored: Exception) {
                // 尝试下一种 su 调用形式
            }
        }
        return false
    }

    private fun showRootDialog() {
        if (isFinishing || isDestroyed) return
        if (rootDialog != null) return
        rootDialog = AlertDialog.Builder(this)
            .setTitle("需要 root 权限")
            .setMessage("Aegis 需要通过 root 读取系统频率并写入调度设置。\n\n" +
                "请在 KernelSU / Magisk 管理器中为 Aegis 授予 root 权限，然后点击“重新检测”。")
            .setPositiveButton("重新检测") { _, _ ->
                rootDialog = null
                rootCheckStarted = false
                checkRootOnce()
            }
            .setNegativeButton("稍后") { _, _ ->
                rootDialog = null
            }
            .setCancelable(false)
            .show()
    }

    /* ---------- shell ---------- */

    private fun runShell(command: String): Triple<Int, String, String> {
        var lastError: Exception? = null
        // On some MIUI builds `su -c` silently demotes to uid 2000. Try the
        // explicit uid form first so config writes really reach /data/adb.
        val attempts = listOf(
            arrayOf("su", "0", "sh", "-c", command),
            arrayOf("su", "-c", command),
            arrayOf("sh", "-c", command)
        )
        for (argv in attempts) {
            try {
                val process = ProcessBuilder(*argv)
                    .directory(File("/"))
                    .redirectErrorStream(true)
                    .start()
                if (!process.waitFor(20, TimeUnit.SECONDS)) {
                    process.destroyForcibly()
                    return Triple(124, "", "命令执行超时")
                }
                val output = process.inputStream.bufferedReader().use { it.readText() }
                return Triple(process.exitValue(), output, "")
            } catch (error: Exception) {
                lastError = error
            }
        }
        return Triple(127, "", lastError?.message ?: "无法执行命令")
    }

    /* ---------- apps ---------- */

    private fun iconBase64(drawable: Drawable): String {
        return try {
            val source = if (drawable is BitmapDrawable && drawable.bitmap != null) {
                drawable.bitmap
            } else {
                val width = if (drawable.intrinsicWidth > 0) drawable.intrinsicWidth else 48
                val height = if (drawable.intrinsicHeight > 0) drawable.intrinsicHeight else 48
                val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
                val canvas = Canvas(bitmap)
                drawable.setBounds(0, 0, width, height)
                drawable.draw(canvas)
                bitmap
            }
            val scaled = Bitmap.createScaledBitmap(source, 40, 40, true)
            val stream = ByteArrayOutputStream()
            scaled.compress(Bitmap.CompressFormat.PNG, 70, stream)
            Base64.encodeToString(stream.toByteArray(), Base64.NO_WRAP)
        } catch (error: Exception) {
            ""
        }
    }

    inner class Bridge {

        private fun reply(id: String, code: Int, stdout: String, stderr: String) {
            val script = "window.AegisShellResolve(" +
                JSONObject.quote(id) + "," + code + "," +
                JSONObject.quote(stdout) + "," + JSONObject.quote(stderr) + ")"
            runOnUiThread { webView.evaluateJavascript(script, null) }
        }

        @JavascriptInterface
        fun exec(command: String, id: String) {
            if (command.isBlank()) {
                reply(id, 2, "", "命令为空")
                return
            }
            executor.execute {
                val result = runShell(command)
                reply(id, result.first, result.second, result.third)
            }
        }

        /**
         * 返回全部已安装应用（用户应用 + 系统应用），带真实名称、图标与分类。
         * 失败时返回 {"apps":[],"error":"..."}，由页面显示原因。
         */
        @JavascriptInterface
        fun apps(): String {
            appsCache?.let { return it }
            val result = JSONObject()
            val array = JSONArray()
            return try {
                val manager = packageManager
                val self = packageName
                val entries = ArrayList<Triple<Boolean, String, JSONObject>>()

                manager.getInstalledApplications(0).forEach { application ->
                    if (application.packageName == self) return@forEach

                    val isSystem = (application.flags and (
                        ApplicationInfo.FLAG_SYSTEM or ApplicationInfo.FLAG_UPDATED_SYSTEM_APP
                    )) != 0

                    val label = try {
                        application.loadLabel(manager).toString().ifBlank { application.packageName }
                    } catch (error: Exception) {
                        application.packageName
                    }

                    val icon = iconCache.getOrPut(application.packageName) {
                        try {
                            iconBase64(application.loadIcon(manager))
                        } catch (error: Exception) {
                            ""
                        }
                    }

                    val item = JSONObject()
                        .put("package", application.packageName)
                        .put("name", label)
                        .put("icon", icon)
                        .put("type", if (isSystem) "system" else "user")
                    entries.add(Triple(isSystem, label.lowercase(), item))
                }

                entries.sortWith(compareBy({ it.first }, { it.second }))
                entries.forEach { array.put(it.third) }
                val json = result.put("apps", array).put("error", JSONObject.NULL).toString()
                appsCache = json
                json
            } catch (error: Exception) {
                result.put("apps", JSONArray())
                    .put("error", "读取应用列表失败：" + (error.message ?: error.javaClass.simpleName))
                    .toString()
            }
        }
    }

    companion object {
        private const val PREFS = "aegis"
        private const val KEY_ROOT_OK = "root_ok"
    }
}

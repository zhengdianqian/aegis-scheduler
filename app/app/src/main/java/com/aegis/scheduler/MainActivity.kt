// Copyright (C) 2026 默默的阿文
package com.aegis.scheduler

import android.app.Activity
import android.app.AlertDialog
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ApplicationInfo
import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.Locale
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

/**
 * Aegis 控制台容器。
 *
 * 界面完全复用 WebUI 的静态产物（assets/webui），原生层只做三件事：
 * 1. 提供 su 命令通道（window.AegisShell.exec），把配置读写落到 /data/adb/tempo；
 * 2. 提供 PackageManager 的全部应用元数据（window.AegisShell.apps）；
 * 3. 开屏检测一次 root 状态，未授权时给出引导。
 */
class MainActivity : Activity() {

    private lateinit var webView: WebView
    private val executor = Executors.newSingleThreadExecutor()
    private val labelExecutor = Executors.newSingleThreadExecutor()
    private val shellRequests = AtomicInteger(0)
    @Volatile private var closing = false
    private val packagesChanged = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            if (closing) return
            labelExecutor.execute {
                appsCache = null
                runOnUiThread {
                    if (!isDestroyed) webView.evaluateJavascript("window.dispatchEvent(new Event('aegispackageschange'))", null)
                }
            }
            scheduleLabelsExport(force = true)
        }
    }
    private var rootCheckStarted = false
    @Volatile private var rootCommand: Array<String>? = null
    private var rootDialog: AlertDialog? = null
    private var appsCache: String? = null
    @Volatile private var labelsLastTry = 0L
    @Volatile private var labelsExportedAt = 0L

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

        registerReceiver(packagesChanged, IntentFilter().apply {
            addAction(Intent.ACTION_PACKAGE_ADDED)
            addAction(Intent.ACTION_PACKAGE_REMOVED)
            addAction(Intent.ACTION_PACKAGE_CHANGED)
            addAction(Intent.ACTION_PACKAGE_REPLACED)
            addDataScheme("package")
        })
        checkRootOnce()
    }

    override fun onResume() {
        super.onResume()
        if (this::webView.isInitialized) {
            webView.onResume()
            webView.evaluateJavascript("window.__AEGIS_FOREGROUND__=true;window.dispatchEvent(new Event('aegisvisibilitychange'))", null)
        }
    }

    override fun onPause() {
        if (this::webView.isInitialized) {
            webView.evaluateJavascript("window.__AEGIS_FOREGROUND__=false;window.dispatchEvent(new Event('aegisvisibilitychange'))", null)
            webView.onPause()
        }
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
        closing = true
        rootDialog?.dismiss()
        rootDialog = null
        unregisterReceiver(packagesChanged)
        executor.shutdownNow()
        labelExecutor.shutdownNow()
        if (this::webView.isInitialized) webView.destroy()
        super.onDestroy()
    }

    /* ---------- root ---------- */

    /**
     * 开屏只检测一次。授权成功会记录到本地，之后启动不再弹窗；
     * 未授权时给出一次明确的授权引导。
     */
    private fun checkRootOnce() {
        if (closing || rootCheckStarted) return
        rootCheckStarted = true
        executor.execute {
            val rooted = hasRoot()
            if (closing || Thread.currentThread().isInterrupted) return@execute
            getSharedPreferences(PREFS, MODE_PRIVATE).edit()
                .putBoolean(KEY_ROOT_OK, rooted)
                .apply()
            if (!rooted) {
                runOnUiThread { showRootDialog() }
            } else {
                // 启动时导出一次应用名映射，供 KernelSU WebUI 的帧反馈页使用。
                scheduleLabelsExport(force = true)
            }
        }
    }

    @Synchronized
    private fun hasRoot(): Boolean {
        if (rootCommand != null) return true
        val attempts = listOf(
            arrayOf("su", "0", "sh", "-c", "id"),
            arrayOf("su", "-c", "id")
        )
        for (argv in attempts) {
            if (closing || Thread.currentThread().isInterrupted) return false
            val result = executeProcess(argv, 8)
            val text = result.second.trim()
            if (result.first == 0 && (text.contains("uid=0") || text == "0")) {
                rootCommand = argv.dropLast(1).toTypedArray()
                return true
            }
        }
        return false
    }

    private fun showRootDialog() {
        if (closing || isFinishing || isDestroyed) return
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
        if (closing) return Triple(125, "", "页面已销毁，拒绝执行命令")
        if (!hasRoot()) return Triple(126, "", "root 控制通道不可用")
        // 业务命令只执行一次，不因失败重试或降级普通 shell。
        return executeProcess(rootCommand!! + command, 20)
    }

    private fun executeProcess(argv: Array<String>, timeoutSeconds: Long): Triple<Int, String, String> {
        if (closing) return Triple(125, "", "页面已销毁，拒绝执行命令")
        val output = StringBuilder()
        val readError = AtomicReference<Exception?>(null)
        var process: Process? = null
        var readerThread: Thread? = null
        fun snapshot(): String = synchronized(output) { output.toString() }
        try {
            if (Thread.currentThread().isInterrupted) throw InterruptedException()
            val running = ProcessBuilder(*argv)
                .directory(File("/"))
                .redirectErrorStream(true)
                .start()
            process = running
            val drainThread = Thread({
                try {
                    running.inputStream.bufferedReader().use { reader ->
                        val buffer = CharArray(4096)
                        while (true) {
                            val count = reader.read(buffer)
                            if (count < 0) break
                            synchronized(output) {
                                val kept = minOf(count, MAX_SHELL_OUTPUT_CHARS - output.length)
                                if (kept > 0) output.append(buffer, 0, kept)
                            }
                        }
                    }
                } catch (error: Exception) {
                    readError.set(error)
                }
            }, "aegis-shell-output").apply {
                isDaemon = true
                start()
            }
            readerThread = drainThread
            running.outputStream.close()
            if (!running.waitFor(timeoutSeconds, TimeUnit.SECONDS)) {
                return Triple(124, snapshot(), "命令执行超时")
            }
            // 子进程可能仍持有输出管道，不无限等待 EOF。
            drainThread.join(1000)
            if (drainThread.isAlive) return Triple(124, snapshot(), "命令输出读取超时")
            readError.get()?.let { return Triple(127, snapshot(), it.message ?: "读取命令输出失败") }
            return Triple(running.exitValue(), snapshot(), "")
        } catch (error: InterruptedException) {
            Thread.currentThread().interrupt()
            return Triple(130, snapshot(), "命令执行已中断")
        } catch (error: Exception) {
            return Triple(127, snapshot(), error.message ?: "无法执行命令")
        } finally {
            // 仅销毁直接启动的进程，不保证回收 su 创建的所有后代进程。
            process?.let { running ->
                runCatching { running.destroy() }
                runCatching { running.destroyForcibly() }
                runCatching { running.outputStream.close() }
                runCatching { running.inputStream.close() }
                runCatching { running.errorStream.close() }
            }
            readerThread?.interrupt()
        }
    }

    /* ---------- labels ---------- */

    /**
     * 导出 `包名 → 应用名` 映射到模块配置目录，供 KernelSU WebUI 的帧反馈页读取。
     * 只包含包名和应用名，不写路径、账号或其他信息。
     */
    private fun scheduleLabelsExport(force: Boolean = false) {
        if (closing || labelExecutor.isShutdown) return
        try {
            labelExecutor.execute {
                if (closing) return@execute
                val now = System.currentTimeMillis()
                if (!force && labelsExportedAt > 0 && now - labelsExportedAt < LABEL_RETRY_MS) return@execute
                if (!force && now - labelsLastTry < LABEL_RETRY_MS) return@execute
                labelsLastTry = now
                exportLabels()
            }
        } catch (ignored: RejectedExecutionException) {
            // 销毁时执行器可能已关闭。
        }
    }

    private fun exportLabels() {
        try {
            val manager = packageManager
            val labels = JSONObject()
            val previousLocale = Locale.getDefault()
            try {
                // 中文优先；缺少中文资源的应用由 loadLabel 自身回退。
                Locale.setDefault(Locale.SIMPLIFIED_CHINESE)
                manager.getInstalledApplications(0).forEach { application ->
                    if (application.packageName == packageName) return@forEach
                    val label = try {
                        application.loadLabel(manager).toString().trim()
                    } catch (ignored: Exception) {
                        ""
                    }
                    if (label.isNotEmpty()) {
                        labels.put(application.packageName, label)
                    }
                }
            } finally {
                Locale.setDefault(previousLocale)
            }

            val payload = JSONObject()
                .put("version", 1)
                .put("updated", System.currentTimeMillis())
                .put("apps", labels)
                .toString()

            val cache = File(cacheDir, "aegis-labels.json")
            cache.writeText(payload, Charsets.UTF_8)
            val source = "'" + cache.absolutePath.replace("'", "'\\''") + "'"
            val command = "mkdir -p /data/adb/tempo && cat " + source +
                " > /data/adb/tempo/labels.json.tmp && chmod 600 /data/adb/tempo/labels.json.tmp" +
                " && mv -f /data/adb/tempo/labels.json.tmp /data/adb/tempo/labels.json"
            val result = runShell(command)
            cache.delete()
            if (result.first == 0) {
                labelsExportedAt = System.currentTimeMillis()
            }
        } catch (ignored: Exception) {
            // 导出失败不影响 APP 内的应用列表
        }
    }

    /* ---------- apps ---------- */

    inner class Bridge {

        private fun reply(id: String, code: Int, stdout: String, stderr: String) {
            val script = "window.AegisShellResolve(" +
                JSONObject.quote(id) + "," + code + "," +
                JSONObject.quote(stdout) + "," + JSONObject.quote(stderr) + ")"
            runOnUiThread { if (!closing && !isDestroyed) webView.evaluateJavascript(script, null) }
        }

        @JavascriptInterface
        fun exec(command: String, id: String) {
            if (closing) {
                reply(id, 125, "", "页面已销毁，拒绝执行命令")
                return
            }
            if (command.isBlank()) {
                reply(id, 2, "", "命令为空")
                return
            }
            while (true) {
                val count = shellRequests.get()
                if (count >= MAX_SHELL_REQUESTS) {
                    reply(id, 125, "", "命令通道忙碌，请稍后重试")
                    return
                }
                if (shellRequests.compareAndSet(count, count + 1)) break
            }
            try {
                executor.execute {
                    try {
                        val result = runShell(command)
                        reply(id, result.first, result.second, result.third)
                    } finally {
                        shellRequests.decrementAndGet()
                    }
                }
            } catch (error: RejectedExecutionException) {
                shellRequests.decrementAndGet()
                reply(id, 125, "", "命令通道已关闭")
            }
        }

        /**
         * 返回全部已安装应用（用户应用 + 系统应用），带真实名称与分类（不批量加载图标）。
         * 失败时返回 {"apps":[],"error":"..."}，由页面显示原因。
         */
        @JavascriptInterface
        fun apps(id: String, force: Boolean) {
            if (closing) return
            try {
                labelExecutor.execute {
                    if (closing) return@execute
                    if (force) appsCache = null
                    val json = appMetadata()
                    reply(id, 0, json, "")
                }
            } catch (error: RejectedExecutionException) {
                reply(id, 125, "", "应用通道已关闭")
            }
        }

        private fun appMetadata(): String {
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

                    val item = JSONObject()
                        .put("package", application.packageName)
                        .put("name", label)
                        .put("type", if (isSystem) "system" else "user")
                    entries.add(Triple(isSystem, label.lowercase(), item))
                }

                entries.sortWith(compareBy({ it.first }, { it.second }))
                entries.forEach { array.put(it.third) }
                val json = result.put("apps", array).put("error", JSONObject.NULL).toString()
                appsCache = json
                scheduleLabelsExport()
                json
            } catch (error: Exception) {
                result.put("apps", JSONArray())
                    .put("error", "读取应用列表失败：" + (error.message ?: error.javaClass.simpleName))
                    .toString()
            }
        }
    }

    companion object {
        private const val MAX_SHELL_REQUESTS = 8
        private const val MAX_SHELL_OUTPUT_CHARS = 1024 * 1024
        private const val PREFS = "aegis"
        private const val KEY_ROOT_OK = "root_ok"
        private const val LABEL_RETRY_MS = 10 * 60 * 1000L
    }
}

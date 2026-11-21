package com.amurcanov.tgwsproxy

import com.sun.jna.Library
import com.sun.jna.Native
import com.sun.jna.Pointer

interface ProxyLibrary : Library {
    companion object {
        val INSTANCE = Native.load("tgwsproxy", ProxyLibrary::class.java) as ProxyLibrary
    }

    fun StartProxy(host: String, port: Int, dcIps: String, secret: String, verbose: Int): Int
    fun StopProxy(): Int
    fun NotifyDefaultNetwork(kind: Int)
    fun NotifyDefaultNetworkWithIdentity(kind: Int, identity: Long)
    fun NotifyDefaultNetworkState(kind: Int, identity: Long, replaced: Int)
    fun SetRouteCacheDir(path: String)
    fun SetStickyRouting(enabled: Int)
    fun RetryFailedRoutes(): Int
    fun GetRouteDiagnostics(): Pointer?
    fun GetProxyStatus(): Int
    fun SetPoolSize(size: Int)
    fun SetBufferSizeKb(kb: Int)
    fun SetCfProxyCacheDir(cacheDir: String)
    fun SetCfProxyConfig(enabled: Int, priority: Int, userDomain: String)
    fun SetCfWorkerDomains(domains: String)
    fun SetCfPlainWs80(enabled: Int)
    fun SetFakeTlsDomain(domain: String)
    fun SetDisableSecure(v: Int)
    fun SetForceTestDc(v: Int)
    fun SetProxyProtocol(v: Int)
    fun GetSecretWithPrefix(): Pointer?
    fun GetStats(): Pointer?
    fun FreeString(p: Pointer)
}

enum class ProxyConnectionState(val label: Int) {
    STOPPED(R.string.status_disconnected),
    LISTENING(R.string.status_listening),
    CONNECTING(R.string.status_upstream_connecting),
    UPSTREAM_VERIFIED(R.string.status_upstream_verified),
    RELAYING_BYTES(R.string.status_relaying),
    DEGRADED(R.string.status_degraded),
    NO_AVAILABLE_ROUTE(R.string.status_no_route),
}

object NativeProxy {
    fun startProxy(host: String, port: Int, dcIps: String, secret: String, verbose: Int): Int {
        return ProxyLibrary.INSTANCE.StartProxy(host, port, dcIps, secret, verbose)
    }

    fun stopProxy(): Int {
        return ProxyLibrary.INSTANCE.StopProxy()
    }

    fun notifyDefaultNetwork(kind: Int, identity: Long, replaced: Boolean) {
        try {
            ProxyLibrary.INSTANCE.NotifyDefaultNetworkState(kind, identity, if (replaced) 1 else 0)
        } catch (_: UnsatisfiedLinkError) {
            try {
                ProxyLibrary.INSTANCE.NotifyDefaultNetworkWithIdentity(kind, identity)
            } catch (_: UnsatisfiedLinkError) {
                try { ProxyLibrary.INSTANCE.NotifyDefaultNetwork(kind) } catch (_: UnsatisfiedLinkError) { }
            }
        }
    }

    fun setRouteCacheDir(path: String) {
        try { ProxyLibrary.INSTANCE.SetRouteCacheDir(path) } catch (_: UnsatisfiedLinkError) { }
    }

    fun setStickyRouting(enabled: Boolean) {
        try { ProxyLibrary.INSTANCE.SetStickyRouting(if (enabled) 1 else 0) } catch (_: UnsatisfiedLinkError) { }
    }

    fun retryFailedRoutes(): Boolean = try {
        ProxyLibrary.INSTANCE.RetryFailedRoutes() == 1
    } catch (_: UnsatisfiedLinkError) { false }

    fun routeDiagnostics(): String? = try {
        ProxyLibrary.INSTANCE.GetRouteDiagnostics()?.let { pointer ->
            try { pointer.getString(0) } finally { ProxyLibrary.INSTANCE.FreeString(pointer) }
        }
    } catch (_: UnsatisfiedLinkError) { null }

    fun connectionState(): ProxyConnectionState = try {
        ProxyConnectionState.entries.getOrElse(ProxyLibrary.INSTANCE.GetProxyStatus()) {
            ProxyConnectionState.LISTENING
        }
    } catch (_: UnsatisfiedLinkError) {
        ProxyConnectionState.LISTENING
    }

    fun setPoolSize(size: Int) {
        ProxyLibrary.INSTANCE.SetPoolSize(size)
    }

    fun setBufferSizeKb(kb: Int) {
        ProxyLibrary.INSTANCE.SetBufferSizeKb(kb)
    }

    fun setCfProxyCacheDir(cacheDir: String) {
        ProxyLibrary.INSTANCE.SetCfProxyCacheDir(cacheDir)
    }

    fun setCfProxyConfig(enabled: Boolean, priority: Boolean, userDomain: String) {
        ProxyLibrary.INSTANCE.SetCfProxyConfig(
            if (enabled) 1 else 0,
            if (priority) 1 else 0,
            userDomain
        )
    }

    fun setCfWorkerDomains(domains: String) {
        ProxyLibrary.INSTANCE.SetCfWorkerDomains(domains)
    }

    fun setCfPlainWs80(enabled: Boolean) {
        ProxyLibrary.INSTANCE.SetCfPlainWs80(if (enabled) 1 else 0)
    }

    fun setFakeTlsDomain(domain: String) {
        ProxyLibrary.INSTANCE.SetFakeTlsDomain(domain)
    }

    fun setDisableSecure(v: Boolean) {
        ProxyLibrary.INSTANCE.SetDisableSecure(if (v) 1 else 0)
    }

    fun setForceTestDc(v: Boolean) {
        ProxyLibrary.INSTANCE.SetForceTestDc(if (v) 1 else 0)
    }

    fun setProxyProtocol(v: Boolean) {
        ProxyLibrary.INSTANCE.SetProxyProtocol(if (v) 1 else 0)
    }

    /** Returns the full secret with correct prefix (dd or ee+domain_hex) */
    fun getSecretWithPrefix(): String? {
        val ptr = ProxyLibrary.INSTANCE.GetSecretWithPrefix() ?: return null
        val res = ptr.getString(0)
        ProxyLibrary.INSTANCE.FreeString(ptr)
        return res
    }

    fun getStats(): String? {
        val ptr = ProxyLibrary.INSTANCE.GetStats() ?: return null
        val res = ptr.getString(0)
        ProxyLibrary.INSTANCE.FreeString(ptr)
        return res
    }
}

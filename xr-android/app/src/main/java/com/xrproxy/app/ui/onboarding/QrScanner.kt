package com.xrproxy.app.ui.onboarding

import android.app.Activity
import com.google.android.gms.common.moduleinstall.ModuleInstall
import com.google.android.gms.common.moduleinstall.ModuleInstallRequest
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.coroutines.suspendCoroutine

/**
 * Launch the Google Code Scanner (system UI, no CAMERA permission needed).
 *
 * Returns the scanned raw value, or `null` if the user cancels or scanning
 * fails. On devices without Play Services we throw so the caller can show
 * the "use Paste link instead" Snackbar.
 */
suspend fun scanInviteQr(activity: Activity): String? =
    suspendCoroutine { cont ->
        val options = GmsBarcodeScannerOptions.Builder()
            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
            .enableAutoZoom()
            .build()
        val scanner = GmsBarcodeScanning.getClient(activity, options)

        // Trigger module install on first use — no-op if already installed.
        ModuleInstall.getClient(activity)
            .installModules(
                ModuleInstallRequest.newBuilder().addApi(scanner).build()
            )

        try {
            scanner.startScan()
                .addOnSuccessListener { barcode -> cont.resume(barcode.rawValue) }
                .addOnFailureListener { cont.resume(null) }
                .addOnCanceledListener { cont.resume(null) }
        } catch (t: Throwable) {
            cont.resumeWithException(t)
        }
    }

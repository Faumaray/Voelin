package io.github.faumaray.tsc

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Saved passwords (server, query), for the Rust side's `Secrets`.
 *
 * Each value is encrypted with AES-256-GCM under a key that never leaves the
 * Android Keystore (hardware-backed where available); the key name is the
 * associated data, so a value cannot be moved to another key. Only the
 * ciphertext is stored, in private preferences excluded from backups.
 */
object SecretStore {
    private const val TAG = "SecretStore"
    private const val KEYSTORE = "AndroidKeyStore"
    private const val KEY_ALIAS = "tsc-secrets"
    private const val PREFS = "secrets"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val IV_BYTES = 12
    private const val TAG_BITS = 128

    private fun key(): SecretKey {
        val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** The value, or null if none is saved or it cannot be decrypted. */
    @Synchronized
    fun get(context: Context, name: String): String? {
        val stored = prefs(context).getString(name, null) ?: return null
        return try {
            val bytes = Base64.decode(stored, Base64.NO_WRAP)
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, bytes, 0, IV_BYTES))
            cipher.updateAAD(name.toByteArray())
            String(cipher.doFinal(bytes, IV_BYTES, bytes.size - IV_BYTES), Charsets.UTF_8)
        } catch (e: Exception) {
            // E.g. the key was lost (the device's lock screen was reset).
            Log.w(TAG, "cannot decrypt $name", e)
            null
        }
    }

    @Synchronized
    fun set(context: Context, name: String, value: String) {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        cipher.updateAAD(name.toByteArray())
        val sealed = cipher.iv + cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        check(cipher.iv.size == IV_BYTES)
        prefs(context).edit().putString(name, Base64.encodeToString(sealed, Base64.NO_WRAP)).commit()
    }

    @Synchronized
    fun delete(context: Context, name: String) {
        prefs(context).edit().remove(name).commit()
    }
}

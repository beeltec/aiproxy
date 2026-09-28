import {
	type PublicKeyCredentialCreationOptionsJSON,
	type PublicKeyCredentialRequestOptionsJSON,
	startAuthentication,
	startRegistration,
} from "@simplewebauthn/browser";

// The server sends the WebAuthn options as plain JSON (webauthn-rs format).

export function createPasskey(options: unknown) {
	return startRegistration({
		optionsJSON: options as PublicKeyCredentialCreationOptionsJSON,
	}).catch(rethrowReadable);
}

export function getPasskey(options: unknown) {
	return startAuthentication({
		optionsJSON: options as PublicKeyCredentialRequestOptionsJSON,
	}).catch(rethrowReadable);
}

function rethrowReadable(error: unknown): never {
	// The browser uses NotAllowedError for "cancelled" and for "timed out".
	if (error instanceof Error && error.name === "NotAllowedError") {
		throw new Error("The passkey request was cancelled or timed out.");
	}
	throw error;
}

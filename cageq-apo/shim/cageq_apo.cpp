/*
    CAGEq's own APO — the C++ shim (filter.md §5.3c).

    This file exists for exactly one reason: an APO must survive audiodg's load handshake,
    and the thing that reliably does is `CBaseAudioProcessingObject` from the Windows SDK
    (format negotiation, connection validation, buffer bookkeeping). It is a C++ class, and
    Rust cannot inherit one. An earlier spike hand-wrote the IAudioProcessingObject vtables
    instead: it CoCreated fine, but audiodg silently discarded it mid-handshake.

    So: this translation unit does COM plumbing and nothing else. Every decision with
    judgement in it — DSP, filter state, the control channel — lives in the Rust static
    library (`../src/lib.rs`) behind the C ABI declared below. Keep it that way; the moment
    audio logic starts leaking in here, the split has stopped paying for itself.

    Structure follows EqualizerAPO's own APO (GPLv2+, the working reference for what
    audiodg accepts), reduced to what CAGEq needs: no child-APO chaining, no config file,
    no logging inside audiodg. Written from its shape, not copied.

    NOTE: `baseaudioprocessingobject.h` / `AudioBaseProcessingObjectV140.lib` ship in the
    plain Windows SDK under `um/` (user-mode), NOT the WDK. No driver kit is required.
*/

#include <windows.h>
#include <unknwn.h>
#include <audioenginebaseapo.h>
#include <baseaudioprocessingobject.h>
#include <audioengineextensionapo.h>

// ---------------------------------------------------------------------------
// The Rust core (../src/lib.rs), linked in statically.
// ---------------------------------------------------------------------------
extern "C" {
void* cageq_apo_create(unsigned int channels, float sampleRate);
void  cageq_apo_destroy(void* handle);
void  cageq_apo_process(void* handle, const float* input, float* output, unsigned int frames);
unsigned long long cageq_apo_frames_processed(void* handle);
float cageq_apo_sample_rate(void* handle);
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

// CAGEq's APO CLSID. MUST stay in sync with scripts/register.ps1.
// {530052E1-2CD4-400A-AC2B-0D19273AD5B7}
static const GUID CLSID_CageqApo =
    { 0x530052E1, 0x2CD4, 0x400A, { 0xAC, 0x2B, 0x0D, 0x19, 0x27, 0x3A, 0xD5, 0xB7 } };

static HINSTANCE g_hModule = nullptr;
static long g_instCount = 0;
static long g_lockCount = 0;

// Registration properties handed to the base class.
//
// The flags are what the engine is promised about this APO, and getting them wrong is a
// classic silent-non-load: FRAMESPERSECOND_MUST_MATCH + BITSPERSAMPLE_MUST_MATCH say
// "input and output formats are identical", INPLACE says "you may pass me the same buffer
// for both" (which is why the Rust side copies with memmove semantics). Same set
// EqualizerAPO declares.
// C4815: with one interface, CRegAPOProperties' trailing `m_aAdditionalAPOIIDs` array is
// zero-sized. That is the SDK template's own shape, not a bug here — its constructor only
// writes that array for interfaces beyond the first, of which there are none. Suppressed
// narrowly around this declaration rather than project-wide.
#pragma warning(push)
#pragma warning(disable: 4815)
static const CRegAPOProperties<1> g_regProperties(
    CLSID_CageqApo,
    L"CAGEq APO",
    L"CAGEq",
    1, 0,
    __uuidof(IAudioProcessingObject),
    (APO_FLAG)(APO_FLAG_FRAMESPERSECOND_MUST_MATCH | APO_FLAG_BITSPERSAMPLE_MUST_MATCH | APO_FLAG_INPLACE));
#pragma warning(pop)

// ---------------------------------------------------------------------------
// The APO
// ---------------------------------------------------------------------------

class CageqApo final : public CBaseAudioProcessingObject, public IAudioSystemEffects
{
public:
    CageqApo() : CBaseAudioProcessingObject(g_regProperties), m_refCount(1), m_rust(nullptr)
    {
        InterlockedIncrement(&g_instCount);
    }

    // IUnknown. Implemented by hand because CBaseAudioProcessingObject deliberately does
    // not: it is `__declspec(novtable)` and leaves lifetime to the subclass. No
    // aggregation support — audiodg does not aggregate system-effects APOs, and the
    // delegating/non-delegating pair EqualizerAPO carries exists only for its child-APO
    // chaining, which CAGEq's APO replaces rather than reimplements.
    STDMETHOD(QueryInterface)(REFIID iid, void** ppv) override
    {
        if (!ppv) return E_POINTER;
        if (iid == __uuidof(IUnknown))                            *ppv = static_cast<IUnknown*>(static_cast<IAudioProcessingObject*>(this));
        else if (iid == __uuidof(IAudioProcessingObject))              *ppv = static_cast<IAudioProcessingObject*>(this);
        else if (iid == __uuidof(IAudioProcessingObjectRT))            *ppv = static_cast<IAudioProcessingObjectRT*>(this);
        else if (iid == __uuidof(IAudioProcessingObjectConfiguration)) *ppv = static_cast<IAudioProcessingObjectConfiguration*>(this);
        else if (iid == __uuidof(IAudioSystemEffects))                 *ppv = static_cast<IAudioSystemEffects*>(this);
        else { *ppv = nullptr; return E_NOINTERFACE; }

        reinterpret_cast<IUnknown*>(*ppv)->AddRef();
        return S_OK;
    }

    STDMETHOD_(ULONG, AddRef)() override { return InterlockedIncrement(&m_refCount); }

    STDMETHOD_(ULONG, Release)() override
    {
        ULONG n = InterlockedDecrement(&m_refCount);
        if (n == 0) delete this;
        return n;
    }

    // IAudioProcessingObject. The base class handles the rest; this only has to accept
    // the system-effects init payload. Validating cbDataSize against the struct we expect
    // is what tells Windows we are a v1 IAudioSystemEffects APO — and was the likely site
    // of the earlier spike's silent rejection.
    STDMETHOD(Initialize)(UINT32 cbDataSize, BYTE* pbyData) override
    {
        if (pbyData == nullptr && cbDataSize != 0) return E_INVALIDARG;
        if (pbyData != nullptr && cbDataSize == 0) return E_POINTER;
        if (cbDataSize != sizeof(APOInitSystemEffects)) return E_INVALIDARG;
        return S_OK;
    }

    // IAudioProcessingObjectConfiguration. Let the base validate and cache the connection
    // (that is the part worth inheriting), then stand up the Rust engine for the format we
    // were actually locked to.
    STDMETHOD(LockForProcess)(
        UINT32 u32NumInputConnections, APO_CONNECTION_DESCRIPTOR** ppInputConnections,
        UINT32 u32NumOutputConnections, APO_CONNECTION_DESCRIPTOR** ppOutputConnections) override
    {
        HRESULT hr = CBaseAudioProcessingObject::LockForProcess(
            u32NumInputConnections, ppInputConnections, u32NumOutputConnections, ppOutputConnections);
        if (FAILED(hr)) return hr;

        m_rust = cageq_apo_create(GetSamplesPerFrame(), GetFramesPerSecond());
        if (m_rust == nullptr)
        {
            // Refuse the lock rather than run mis-configured: Windows then drops this APO
            // cleanly and the endpoint keeps working, which is the failure mode to want
            // when the alternative lives on the RT thread of the whole machine's audio.
            CBaseAudioProcessingObject::UnlockForProcess();
            return E_INVALIDARG;
        }
        return S_OK;
    }

    STDMETHOD(UnlockForProcess)() override
    {
        cageq_apo_destroy(m_rust);
        m_rust = nullptr;
        return CBaseAudioProcessingObject::UnlockForProcess();
    }

    // IAudioProcessingObjectRT — the real-time callback. Nothing here may allocate, lock
    // or block; see the Rust side's own real-time contract.
    #pragma AVRT_CODE_BEGIN
    STDMETHOD_(void, APOProcess)(
        UINT32 u32NumInputConnections, APO_CONNECTION_PROPERTY** ppInputConnections,
        UINT32 u32NumOutputConnections, APO_CONNECTION_PROPERTY** ppOutputConnections) override
    {
        UNREFERENCED_PARAMETER(u32NumInputConnections);
        UNREFERENCED_PARAMETER(u32NumOutputConnections);

        APO_CONNECTION_PROPERTY* in = ppInputConnections[0];
        APO_CONNECTION_PROPERTY* out = ppOutputConnections[0];

        switch (in->u32BufferFlags)
        {
        case BUFFER_VALID:
            cageq_apo_process(m_rust,
                              reinterpret_cast<const float*>(in->pBuffer),
                              reinterpret_cast<float*>(out->pBuffer),
                              in->u32ValidFrameCount);
            out->u32ValidFrameCount = in->u32ValidFrameCount;
            out->u32BufferFlags = BUFFER_VALID;
            break;

        case BUFFER_SILENT:
            // Pass silence through as silence. Some drivers treat BUFFER_SILENT as
            // meaningful, and a stage-B identity APO has no reason to manufacture signal
            // where the engine promised none. (Once real filters exist, a decaying tail
            // means this can no longer be a pure passthrough — stage C's problem.)
            out->u32ValidFrameCount = in->u32ValidFrameCount;
            out->u32BufferFlags = BUFFER_SILENT;
            break;

        default:
            out->u32ValidFrameCount = 0;
            out->u32BufferFlags = in->u32BufferFlags;
            break;
        }
    }
    #pragma AVRT_CODE_END

private:
    ~CageqApo() { InterlockedDecrement(&g_instCount); }

    long  m_refCount;
    void* m_rust; // opaque handle from cageq_apo_create
};

// ---------------------------------------------------------------------------
// Class factory
// ---------------------------------------------------------------------------

class CageqApoFactory final : public IClassFactory
{
public:
    CageqApoFactory() : m_refCount(1) {}

    STDMETHOD(QueryInterface)(REFIID iid, void** ppv) override
    {
        if (!ppv) return E_POINTER;
        if (iid == __uuidof(IUnknown) || iid == __uuidof(IClassFactory))
        {
            *ppv = static_cast<IClassFactory*>(this);
            AddRef();
            return S_OK;
        }
        *ppv = nullptr;
        return E_NOINTERFACE;
    }

    STDMETHOD_(ULONG, AddRef)() override { return InterlockedIncrement(&m_refCount); }

    STDMETHOD_(ULONG, Release)() override
    {
        ULONG n = InterlockedDecrement(&m_refCount);
        if (n == 0) delete this;
        return n;
    }

    STDMETHOD(CreateInstance)(IUnknown* pOuter, REFIID iid, void** ppv) override
    {
        if (!ppv) return E_POINTER;
        *ppv = nullptr;
        if (pOuter != nullptr) return CLASS_E_NOAGGREGATION;

        CageqApo* apo = new (std::nothrow) CageqApo();
        if (apo == nullptr) return E_OUTOFMEMORY;

        HRESULT hr = apo->QueryInterface(iid, ppv);
        apo->Release();
        return hr;
    }

    STDMETHOD(LockServer)(BOOL lock) override
    {
        if (lock) InterlockedIncrement(&g_lockCount); else InterlockedDecrement(&g_lockCount);
        return S_OK;
    }

private:
    ~CageqApoFactory() = default;
    long m_refCount;
};

// ---------------------------------------------------------------------------
// DLL exports
// ---------------------------------------------------------------------------

BOOL WINAPI DllMain(HINSTANCE hModule, DWORD reason, void*)
{
    if (reason == DLL_PROCESS_ATTACH)
    {
        g_hModule = hModule;
        DisableThreadLibraryCalls(hModule);
    }
    return TRUE;
}

STDAPI DllCanUnloadNow()
{
    return (g_instCount == 0 && g_lockCount == 0) ? S_OK : S_FALSE;
}

STDAPI DllGetClassObject(REFCLSID clsid, REFIID iid, void** ppv)
{
    if (clsid != CLSID_CageqApo) return CLASS_E_CLASSNOTAVAILABLE;

    CageqApoFactory* factory = new (std::nothrow) CageqApoFactory();
    if (factory == nullptr) return E_OUTOFMEMORY;

    HRESULT hr = factory->QueryInterface(iid, ppv);
    factory->Release();
    return hr;
}

STDAPI DllRegisterServer()
{
    // Two registrations, both required: `RegisterAPO` publishes the APO's properties to
    // the audio engine, and the CLSID/InprocServer32 keys are what lets COM create it at
    // all. Registering only one is another silent-non-load.
    // Passed by object, not by address: CRegAPOProperties supplies an implicit
    // `operator const APO_REG_PROPERTIES*` that unwraps to the inner struct. `&g_regProperties`
    // would be a pointer to the *wrapper* and does not convert.
    HRESULT hr = RegisterAPO(g_regProperties);
    if (FAILED(hr)) return hr;

    wchar_t path[MAX_PATH] = {};
    if (GetModuleFileNameW(g_hModule, path, ARRAYSIZE(path)) == 0)
    {
        UnregisterAPO(CLSID_CageqApo);
        return HRESULT_FROM_WIN32(GetLastError());
    }

    wchar_t clsidText[64] = {};
    if (StringFromGUID2(CLSID_CageqApo, clsidText, ARRAYSIZE(clsidText)) == 0)
    {
        UnregisterAPO(CLSID_CageqApo);
        return E_UNEXPECTED;
    }

    wchar_t keyPath[256] = {};
    HKEY key = nullptr;
    wsprintfW(keyPath, L"SOFTWARE\\Classes\\CLSID\\%s\\InprocServer32", clsidText);

    LSTATUS st = RegCreateKeyExW(HKEY_LOCAL_MACHINE, keyPath, 0, nullptr,
                                 REG_OPTION_NON_VOLATILE, KEY_WRITE, nullptr, &key, nullptr);
    if (st != ERROR_SUCCESS)
    {
        UnregisterAPO(CLSID_CageqApo);
        return HRESULT_FROM_WIN32(st);
    }

    const DWORD pathBytes = static_cast<DWORD>((lstrlenW(path) + 1) * sizeof(wchar_t));
    st = RegSetValueExW(key, nullptr, 0, REG_SZ, reinterpret_cast<const BYTE*>(path), pathBytes);
    if (st == ERROR_SUCCESS)
    {
        // "Both" so audiodg can create it on whichever apartment it uses.
        static const wchar_t kBoth[] = L"Both";
        st = RegSetValueExW(key, L"ThreadingModel", 0, REG_SZ,
                            reinterpret_cast<const BYTE*>(kBoth), sizeof(kBoth));
    }
    RegCloseKey(key);

    if (st != ERROR_SUCCESS)
    {
        UnregisterAPO(CLSID_CageqApo);
        return HRESULT_FROM_WIN32(st);
    }
    return S_OK;
}

STDAPI DllUnregisterServer()
{
    wchar_t clsidText[64] = {};
    if (StringFromGUID2(CLSID_CageqApo, clsidText, ARRAYSIZE(clsidText)) != 0)
    {
        wchar_t keyPath[256] = {};
        wsprintfW(keyPath, L"SOFTWARE\\Classes\\CLSID\\%s\\InprocServer32", clsidText);
        RegDeleteKeyW(HKEY_LOCAL_MACHINE, keyPath);

        wsprintfW(keyPath, L"SOFTWARE\\Classes\\CLSID\\%s", clsidText);
        RegDeleteKeyW(HKEY_LOCAL_MACHINE, keyPath);
    }
    return UnregisterAPO(CLSID_CageqApo);
}

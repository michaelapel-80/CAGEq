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
#include <stdio.h>
#include <stdarg.h>

// ---------------------------------------------------------------------------
// Diagnostics
//
// Format negotiation is the one part of an APO that fails *silently* — Windows just
// reports "format not supported" and gives up, with nothing in the event log naming which
// call refused. So the negotiation path logs what it was asked and what it answered.
//
// Writes to %TEMP%\CAGEqApo.log. Inside audiodg that resolves under LocalService's own
// profile, which is where Equalizer APO's own logger writes from the same process — i.e. a
// path proven writable in this exact security context.
//
// NEVER called from APOProcess: that is the real-time thread, where file I/O would be a
// dropout for the whole machine. Config/negotiation calls are not real-time.
// ---------------------------------------------------------------------------
#define CAGEQ_APO_DIAG 1

#if CAGEQ_APO_DIAG
static void DiagF(_In_z_ _Printf_format_string_ const wchar_t* fmt, ...)
{
    wchar_t path[MAX_PATH];
    if (GetTempPathW(ARRAYSIZE(path), path) == 0) return;
    if (FAILED(StringCchCatW(path, ARRAYSIZE(path), L"CAGEqApo.log"))) return;

    FILE* fp = nullptr;
    if (_wfopen_s(&fp, path, L"at, ccs=UTF-8") != 0 || fp == nullptr) return;

    SYSTEMTIME st;
    GetLocalTime(&st);
    fwprintf(fp, L"[%02u:%02u:%02u.%03u pid=%lu] ", st.wHour, st.wMinute, st.wSecond,
             st.wMilliseconds, GetCurrentProcessId());

    va_list va;
    va_start(va, fmt);
    vfwprintf(fp, fmt, va);
    va_end(va);

    fwprintf(fp, L"\n");
    fclose(fp);
}

/// Describe an IAudioMediaType. The five fields that decide whether an APO accepts a
/// format, so a rejection can be read off the log rather than guessed at.
static void DiagFormat(const wchar_t* label, IAudioMediaType* type)
{
    if (type == nullptr) { DiagF(L"    %s = (null)", label); return; }

    UNCOMPRESSEDAUDIOFORMAT f = {};
    HRESULT hr = type->GetUncompressedAudioFormat(&f);
    if (FAILED(hr)) { DiagF(L"    %s = GetUncompressedAudioFormat failed 0x%08X", label, hr); return; }

    DiagF(L"    %s = subtype:%08X ch:%u container:%uB validBits:%u rate:%.1f mask:%08X",
          label, f.guidFormatType.Data1, f.dwSamplesPerFrame, f.dwBytesPerSampleContainer,
          f.dwValidBitsPerSample, f.fFramesPerSecond, f.dwChannelMask);
}
#else
#define DiagF(...)        ((void)0)
#define DiagFormat(a, b)  ((void)0)
#endif

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

/// The object's *own* IUnknown, kept separate from the one its interfaces expose.
///
/// COM aggregation splits IUnknown in two: the interfaces an aggregated object hands out
/// must forward AddRef/Release/QueryInterface to the **outer** object, so the whole
/// aggregate looks like one COM identity — but something still has to control this
/// object's own lifetime, and that is this. Deliberately laid out QI/AddRef/Release in the
/// same slots as IUnknown, so a pointer to it can be reinterpret_cast to IUnknown* (the
/// standard trick, and what EqualizerAPO does).
class INonDelegatingUnknown
{
public:
    virtual HRESULT __stdcall NonDelegatingQueryInterface(REFIID iid, void** ppv) = 0;
    virtual ULONG   __stdcall NonDelegatingAddRef() = 0;
    virtual ULONG   __stdcall NonDelegatingRelease() = 0;
};

class CageqApo final : public CBaseAudioProcessingObject, public IAudioSystemEffects, public INonDelegatingUnknown
{
public:
    /// `pOuter` is the aggregating object, or null when created standalone. **audiodg does
    /// aggregate system-effects APOs** — measured on the VM: `CreateInstance` arrives with a
    /// non-null outer and `IID_IUnknown`, and an earlier version of this file refused it with
    /// CLASS_E_NOAGGREGATION on the assumption that aggregation never happened. The APO was
    /// then simply never constructed: no Initialize, no LockForProcess, a dead endpoint, and
    /// nothing in any log but a retry loop. That assumption is why EqualizerAPO carries this
    /// same delegating/non-delegating machinery.
    explicit CageqApo(IUnknown* pOuter)
        : CBaseAudioProcessingObject(g_regProperties), m_refCount(1), m_rust(nullptr)
    {
        // Standalone: delegate to ourselves, so the delegating methods below need no branch.
        m_pUnkOuter = (pOuter != nullptr)
            ? pOuter
            : reinterpret_cast<IUnknown*>(static_cast<INonDelegatingUnknown*>(this));
        DiagF(L"CageqApo CONSTRUCTED (aggregated=%s, instances now %ld)",
              (pOuter != nullptr) ? L"yes" : L"no", InterlockedIncrement(&g_instCount));
    }

    // --- Delegating IUnknown: what every interface on this object exposes. Forwards to the
    // aggregate's identity (or to ourselves when standalone).
    STDMETHOD(QueryInterface)(REFIID iid, void** ppv) override { return m_pUnkOuter->QueryInterface(iid, ppv); }
    STDMETHOD_(ULONG, AddRef)() override { return m_pUnkOuter->AddRef(); }
    STDMETHOD_(ULONG, Release)() override { return m_pUnkOuter->Release(); }

    // --- Non-delegating IUnknown: this object's real identity and lifetime.
    // Implemented by hand because CBaseAudioProcessingObject deliberately does not: it is
    // `__declspec(novtable)` and leaves lifetime to the subclass.
    STDMETHOD(NonDelegatingQueryInterface)(REFIID iid, void** ppv) override
    {
        if (!ppv) return E_POINTER;

        // IID_IUnknown must yield the NON-delegating unknown: that is the pointer the
        // aggregator holds to control our lifetime, and handing back a delegating one here
        // would make us forward our own lifetime to the outer object — an immediate cycle.
        if (iid == __uuidof(IUnknown))                                 *ppv = static_cast<INonDelegatingUnknown*>(this);
        else if (iid == __uuidof(IAudioProcessingObject))              *ppv = static_cast<IAudioProcessingObject*>(this);
        else if (iid == __uuidof(IAudioProcessingObjectRT))            *ppv = static_cast<IAudioProcessingObjectRT*>(this);
        else if (iid == __uuidof(IAudioProcessingObjectConfiguration)) *ppv = static_cast<IAudioProcessingObjectConfiguration*>(this);
        else if (iid == __uuidof(IAudioSystemEffects))                 *ppv = static_cast<IAudioSystemEffects*>(this);
        else { *ppv = nullptr; return E_NOINTERFACE; }

        reinterpret_cast<IUnknown*>(*ppv)->AddRef();
        return S_OK;
    }

    STDMETHOD_(ULONG, NonDelegatingAddRef)() override { return InterlockedIncrement(&m_refCount); }

    STDMETHOD_(ULONG, NonDelegatingRelease)() override
    {
        ULONG n = InterlockedDecrement(&m_refCount);
        if (n == 0) { delete this; return 0; }
        return n;
    }

    // IAudioProcessingObject. The base class handles the rest; this only has to accept
    // the system-effects init payload. Validating cbDataSize against the struct we expect
    // is what tells Windows we are a v1 IAudioSystemEffects APO — and was the likely site
    // of the earlier spike's silent rejection.
    STDMETHOD(Initialize)(UINT32 cbDataSize, BYTE* pbyData) override
    {
        // Log the sizes before judging: Windows can hand a v1/v2/v3 init struct depending on
        // which IAudioSystemEffects interface the APO advertises, and rejecting the wrong one
        // is invisible from outside.
        DiagF(L"Initialize: cbDataSize=%u (v1=%zu)", cbDataSize, sizeof(APOInitSystemEffects));

        if (pbyData == nullptr && cbDataSize != 0) return E_INVALIDARG;
        if (pbyData != nullptr && cbDataSize == 0) return E_POINTER;
        if (cbDataSize != sizeof(APOInitSystemEffects)) {
            DiagF(L"  -> E_INVALIDARG (unexpected init struct size)");
            return E_INVALIDARG;
        }
        return S_OK;
    }

    // Format negotiation. Both are pure delegation to the base class — overridden ONLY so the
    // question and the answer end up in the log; remove once negotiation is understood.
    STDMETHOD(IsInputFormatSupported)(IAudioMediaType* pOutputFormat,
                                      IAudioMediaType* pRequestedInputFormat,
                                      IAudioMediaType** ppSupportedInputFormat) override
    {
        DiagF(L"IsInputFormatSupported");
        DiagFormat(L"output   ", pOutputFormat);
        DiagFormat(L"requested", pRequestedInputFormat);

        HRESULT hr = CBaseAudioProcessingObject::IsInputFormatSupported(
            pOutputFormat, pRequestedInputFormat, ppSupportedInputFormat);

        DiagF(L"  -> 0x%08X%s", hr, (hr == S_FALSE) ? L" (S_FALSE = rejected, suggesting another)" : L"");
        if (hr == S_FALSE && ppSupportedInputFormat != nullptr) DiagFormat(L"suggested", *ppSupportedInputFormat);
        return hr;
    }

    STDMETHOD(IsOutputFormatSupported)(IAudioMediaType* pInputFormat,
                                       IAudioMediaType* pRequestedOutputFormat,
                                       IAudioMediaType** ppSupportedOutputFormat) override
    {
        DiagF(L"IsOutputFormatSupported");
        DiagFormat(L"input    ", pInputFormat);
        DiagFormat(L"requested", pRequestedOutputFormat);

        HRESULT hr = CBaseAudioProcessingObject::IsOutputFormatSupported(
            pInputFormat, pRequestedOutputFormat, ppSupportedOutputFormat);

        DiagF(L"  -> 0x%08X%s", hr, (hr == S_FALSE) ? L" (S_FALSE = rejected, suggesting another)" : L"");
        if (hr == S_FALSE && ppSupportedOutputFormat != nullptr) DiagFormat(L"suggested", *ppSupportedOutputFormat);
        return hr;
    }

    // IAudioProcessingObjectConfiguration. Let the base validate and cache the connection
    // (that is the part worth inheriting), then stand up the Rust engine for the format we
    // were actually locked to.
    STDMETHOD(LockForProcess)(
        UINT32 u32NumInputConnections, APO_CONNECTION_DESCRIPTOR** ppInputConnections,
        UINT32 u32NumOutputConnections, APO_CONNECTION_DESCRIPTOR** ppOutputConnections) override
    {
        DiagF(L"LockForProcess: in=%u out=%u", u32NumInputConnections, u32NumOutputConnections);
        if (u32NumInputConnections < 1 || u32NumOutputConnections < 1 ||
            ppInputConnections == nullptr || ppOutputConnections == nullptr)
        {
            return APOERR_NUM_CONNECTIONS_INVALID;
        }
        DiagFormat(L"conn in  ", ppInputConnections[0]->pFormat);
        DiagFormat(L"conn out ", ppOutputConnections[0]->pFormat);

        // Read the format from the CONNECTION DESCRIPTOR, not from the base class's
        // GetSamplesPerFrame().
        //
        // This is the fix for a bug that made the APO load but never process: the base only
        // caches `m_u32SamplesPerFrame` when `APO_FLAG_SAMPLESPERFRAME_MUST_MATCH` is
        // declared (see baseaudioprocessingobject.h's own comment on that member), and we
        // deliberately do not declare it. So GetSamplesPerFrame() returned 0, the Rust
        // engine refused to be created, LockForProcess failed — and Windows quietly dropped
        // us from the chain and played the audio unprocessed. Audible success, total
        // functional failure, no error anywhere.
        //
        // The descriptor is authoritative and unconditional, and is what EqualizerAPO reads
        // for the same purpose. `GetFramesPerSecond()` happened to work (we DO declare
        // FRAMESPERSECOND_MUST_MATCH) but is read from the same place now, so the two
        // cannot disagree.
        UNCOMPRESSEDAUDIOFORMAT inFormat = {};
        HRESULT hr = ppInputConnections[0]->pFormat->GetUncompressedAudioFormat(&inFormat);
        if (FAILED(hr))
        {
            DiagF(L"  -> GetUncompressedAudioFormat FAILED 0x%08X", hr);
            return hr;
        }

        hr = CBaseAudioProcessingObject::LockForProcess(
            u32NumInputConnections, ppInputConnections, u32NumOutputConnections, ppOutputConnections);
        if (FAILED(hr))
        {
            DiagF(L"  -> base LockForProcess FAILED 0x%08X", hr);
            return hr;
        }

        const UINT32 channels = inFormat.dwSamplesPerFrame;
        const FLOAT32 rate = inFormat.fFramesPerSecond;
        DiagF(L"  base ok; channels=%u rate=%.1f", channels, rate);

        m_rust = cageq_apo_create(channels, rate);
        if (m_rust == nullptr)
        {
            DiagF(L"  -> cageq_apo_create REFUSED channels=%u rate=%.1f", channels, rate);
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
        // The frame counter is the ONLY positive evidence that this APO actually processed
        // audio. With an identity passthrough, "it loads, it locks, audio plays" is equally
        // true of an APO that Windows dropped from the chain entirely — which is exactly how
        // an earlier bug here went unnoticed and got called a pass. A non-zero count that
        // scales with playback duration cannot be produced any other way: APOProcess ran, on
        // our handle, on the real-time thread.
        //
        // Read here rather than on the RT path, where logging would be a dropout.
        DiagF(L"UnlockForProcess: frames processed this lock = %llu",
              cageq_apo_frames_processed(m_rust));

        // Detach BEFORE destroying, not after. The engine is documented not to call
        // APOProcess concurrently with this, but the reversed order leaves a window where
        // m_rust is a dangling pointer that APOProcess's null check would happily accept —
        // a use-after-free on the real-time thread of the whole machine's audio. Cheap to
        // order correctly; catastrophic and near-undebuggable if the assumption ever fails.
        void* rust = m_rust;
        m_rust = nullptr;
        cageq_apo_destroy(rust);
        return CBaseAudioProcessingObject::UnlockForProcess();
    }

    // Overridden only to log — the base's implementation is what we want (zero added
    // latency). Worth seeing, because it is called during stream setup and an error here
    // can fail the whole stream before format negotiation is ever reached.
    STDMETHOD(GetLatency)(HNSTIME* pTime) override
    {
        HRESULT hr = CBaseAudioProcessingObject::GetLatency(pTime);
        DiagF(L"GetLatency -> 0x%08X (%lld)", hr, (pTime != nullptr) ? *pTime : -1);
        return hr;
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
    ~CageqApo() { DiagF(L"CageqApo DESTROYED (instances now %ld)", InterlockedDecrement(&g_instCount)); }

    long      m_refCount;
    IUnknown* m_pUnkOuter; // aggregating identity, or ourselves when standalone
    void*     m_rust;      // opaque handle from cageq_apo_create
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
        // pOuter is the whole question here: if audiodg ever asks to AGGREGATE this APO, the
        // CLASS_E_NOAGGREGATION below refuses and the object is never constructed — which
        // from outside looks exactly like the current symptom (DllGetClassObject called
        // repeatedly, no constructor, no Initialize, no LockForProcess, dead endpoint).
        // EqualizerAPO carries a full delegating/non-delegating IUnknown pair, which is
        // strong evidence that aggregation IS used on this path; a comment in this file
        // previously asserted the opposite without evidence.
        DiagF(L"ClassFactory::CreateInstance: pOuter=%s iid=%08X-%04X-%04X",
              (pOuter == nullptr) ? L"NULL (standalone)" : L"NON-NULL (aggregated)",
              iid.Data1, iid.Data2, iid.Data3);

        if (!ppv) return E_POINTER;
        *ppv = nullptr;

        // COM's rule for aggregation: an aggregated object may only be asked for
        // IID_IUnknown at creation, because the aggregator wants the non-delegating unknown
        // and nothing else. Any other interface comes later, through that pointer.
        if (pOuter != nullptr && iid != __uuidof(IUnknown))
        {
            DiagF(L"  -> E_NOINTERFACE (aggregation may only request IID_IUnknown)");
            return E_NOINTERFACE;
        }

        CageqApo* apo = new (std::nothrow) CageqApo(pOuter);
        if (apo == nullptr) return E_OUTOFMEMORY;

        // Non-delegating throughout: the object is born with one non-delegating reference,
        // so it must be queried and released the same way — going through the delegating
        // pair here would hand our lifetime to the outer object before it even holds us.
        HRESULT hr = apo->NonDelegatingQueryInterface(iid, ppv);
        apo->NonDelegatingRelease();
        DiagF(L"  -> 0x%08X", hr);
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
    // Logged because it is the FIRST thing that happens if the engine is trying to use us at
    // all. Silence here on a format change means the rejection is upstream of this APO
    // entirely — we were never consulted — which is a very different bug from us saying no.
    DiagF(L"DllGetClassObject: clsid=%08X-%04X-%04X requested",
          clsid.Data1, clsid.Data2, clsid.Data3);

    if (clsid != CLSID_CageqApo)
    {
        DiagF(L"  -> CLASS_E_CLASSNOTAVAILABLE (not our CLSID)");
        return CLASS_E_CLASSNOTAVAILABLE;
    }

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

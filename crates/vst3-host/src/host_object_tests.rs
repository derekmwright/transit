//! Host SDK contracts, exercised through the generated COM ABI. No installed plug-ins/devices.

use super::*;

/// # Safety
/// object must be live; IID/output must be null or readable/writable as documented by our QI.
unsafe fn query(object: *mut FUnknown, iid: *const sb::TUID, out: *mut *mut c_void) -> sb::tresult {
    // SAFETY: forwards the live interface and QI argument contract through its SDK vtable.
    unsafe { ((*(*object).vtbl).queryInterface)(object, iid, out) }
}

/// # Safety
/// object must point to a live single-interface SDK object held by the caller.
unsafe fn add_ref<I: Interface>(object: *mut I) -> u32 {
    let object = object.cast::<FUnknown>();
    // SAFETY: tested interfaces have FUnknown at offset zero; the caller holds a live reference.
    unsafe { ((*(*object).vtbl).addRef)(object) }
}

/// # Safety
/// object must point to a live single-interface SDK object; the caller surrenders one owned ref.
unsafe fn release<I: Interface>(object: *mut I) -> u32 {
    let object = object.cast::<FUnknown>();
    // SAFETY: tested interfaces have FUnknown at offset zero; this call releases one owned ref.
    unsafe { ((*(*object).vtbl).release)(object) }
}

/// Fail host allocation number n (zero based) and restore the previous setting even on unwind.
fn fail_after<T>(n: usize, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<usize>);
    impl Drop for Reset {
        fn drop(&mut self) {
            HOST_ALLOC_FAIL_AFTER.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(HOST_ALLOC_FAIL_AFTER.with(|value| value.replace(Some(n))));
    run()
}

fn create(host: &ComWrapper<HostContext>, cid: [u8; 16], iid: [u8; 16]) -> (sb::tresult, *mut c_void) {
    let host = host.as_com_ref::<sv::IHostApplication>().expect("host interface");
    let (mut cid, mut iid) = (cid.map(|b| b as c_char), iid.map(|b| b as c_char));
    let mut out = std::ptr::dangling_mut::<c_void>();
    // SAFETY: live host, readable TUIDs, distinct writable output; dispatches through the ABI.
    let result = unsafe { host.createInstance(&mut cid, &mut iid, &mut out) };
    (result, out)
}

fn create_as<I: Interface>(host: &ComWrapper<HostContext>) -> ComPtr<I> {
    let (result, out) = create(host, I::IID, I::IID);
    assert_eq!(result, sb::kResultOk);
    // SAFETY: successful creation transfers one reference to the requested interface.
    unsafe { ComPtr::from_raw(out.cast::<I>()) }.expect("created interface")
}

#[test]
fn allocation_failure_during_creation_clears_output() {
    let host = ComWrapper::new(HostContext::default());
    for (cid, allocations) in [(sv::IAttributeList::IID, 1), (sv::IMessage::IID, 2)] {
        for iid in [cid, FUnknown::IID] {
            for allocation in 0..allocations {
                assert_eq!(fail_after(allocation, || create(&host, cid, iid)), (sb::kOutOfMemory, std::ptr::null_mut()));
            }
            let (result, out) = fail_after(allocations, || create(&host, cid, iid));
            assert_eq!(result, sb::kResultOk);
            // SAFETY: both supported interfaces derive from FUnknown at offset zero; adopt once.
            drop(unsafe { ComPtr::<FUnknown>::from_raw(out.cast()) }.expect("one creation reference"));
        }
    }
}

#[test]
fn allocation_failure_in_values_preserves_previous_attributes() {
    let host = ComWrapper::new(HostContext::default());
    let list = create_as::<sv::IAttributeList>(&host);
    let old = [1u8, 2, 3];
    let replacement = [7u8; 4096];
    let text = [b'x' as u16, 0];
    // SAFETY: list stays owned, keys/text are terminated, byte inputs and outputs have exact sizes.
    unsafe {
        assert_eq!(list.setBinary(c"binary".as_ptr(), old.as_ptr().cast(), old.len() as u32), sb::kResultOk);
        assert_eq!(list.setString(c"string".as_ptr(), text.as_ptr()), sb::kResultOk);
        for key in [c"binary", c"absent"] {
            assert_eq!(fail_after(0, || list.setBinary(key.as_ptr(), replacement.as_ptr().cast(), 4096)), sb::kOutOfMemory);
        }
        for key in [c"string", c"absent"] {
            assert_eq!(fail_after(0, || list.setString(key.as_ptr(), [b'y' as u16, 0].as_ptr())), sb::kOutOfMemory);
        }
        let (mut data, mut size) = (std::ptr::null(), 0);
        assert_eq!(list.getBinary(c"binary".as_ptr(), &mut data, &mut size), sb::kResultOk);
        assert_eq!(size, 3);
        assert_eq!(std::slice::from_raw_parts(data.cast::<u8>(), size as usize), old);
        assert_eq!(list.getBinary(c"absent".as_ptr(), &mut data, &mut size), sb::kResultFalse);
        let mut out = [0u16; 2];
        assert_eq!(list.getString(c"string".as_ptr(), out.as_mut_ptr(), 4), sb::kResultOk);
        assert_eq!(out, text);
        assert_eq!(list.getString(c"absent".as_ptr(), out.as_mut_ptr(), 4), sb::kResultFalse);
    }
}

#[test]
fn allocation_failure_in_keys_and_map_stores_nothing() {
    let host = ComWrapper::new(HostContext::default());
    for allocation in 0..2 {
        let list = create_as::<sv::IAttributeList>(&host);
        // SAFETY: live list, terminated ID and writable scalar output; failed insert must be atomic.
        unsafe {
            assert_eq!(fail_after(allocation, || list.setInt(c"new".as_ptr(), 42)), sb::kOutOfMemory);
            let mut out = -1;
            assert_eq!(list.getInt(c"new".as_ptr(), &mut out), sb::kResultFalse);
            assert_eq!(out, -1);
            assert_eq!(list.setInt(c"new".as_ptr(), 42), sb::kResultOk);
            assert_eq!(list.getInt(c"new".as_ptr(), &mut out), sb::kResultOk);
            assert_eq!(out, 42);
        }
    }
    // Force growth of a nonempty map too: no failure may disturb an existing value.
    let list = HostAttributeList::default();
    // SAFETY: all IDs below are terminated and live for the calls; out is a writable scalar.
    unsafe {
        assert_eq!(list.setInt(c"old".as_ptr(), 7), sb::kResultOk);
        let capacity = list.attrs.lock().expect("attributes").capacity();
        for n in 1..capacity {
            let key = std::ffi::CString::new(format!("key{n}")).expect("key");
            assert_eq!(list.setInt(key.as_ptr(), 0), sb::kResultOk);
        }
        assert_eq!(fail_after(1, || list.setFloat(c"new".as_ptr(), 0.5)), sb::kOutOfMemory);
        assert_eq!(list.attrs.lock().expect("attributes").len(), capacity);
        let mut out = 0;
        assert_eq!(list.getInt(c"old".as_ptr(), &mut out), sb::kResultOk);
        assert_eq!(out, 7);
    }
}

#[test]
fn allocation_failure_in_message_id_preserves_old_id() {
    let host = ComWrapper::new(HostContext::default());
    let msg = create_as::<sv::IMessage>(&host);
    // SAFETY: owned message; literal IDs are terminated and each borrowed ID is read before mutation.
    unsafe {
        fail_after(0, || msg.setMessageID(c"new".as_ptr()));
        assert!(msg.getMessageID().is_null());
        msg.setMessageID(c"old".as_ptr());
        fail_after(0, || msg.setMessageID(c"new".as_ptr()));
        assert_eq!(std::ffi::CStr::from_ptr(msg.getMessageID()), c"old");
        msg.setMessageID(msg.getMessageID());
        assert_eq!(std::ffi::CStr::from_ptr(msg.getMessageID()), c"old");
        fail_after(0, || msg.setMessageID(std::ptr::null()));
        assert!(msg.getMessageID().is_null());
    }
}

#[test]
fn getters_and_scalar_replacement_need_no_allocation() {
    let host = ComWrapper::new(HostContext::default());
    let list = create_as::<sv::IAttributeList>(&host);
    // SAFETY: live list, terminated IDs/text, and distinct correctly sized output buffers.
    unsafe {
        assert_eq!(list.setString(c"s".as_ptr(), [b'x' as u16, 0].as_ptr()), sb::kResultOk);
        assert_eq!(list.setInt(c"i".as_ptr(), 1), sb::kResultOk);
        assert_eq!(list.setFloat(c"f".as_ptr(), 0.5), sb::kResultOk);
        assert_eq!(list.setBinary(c"b".as_ptr(), [7u8].as_ptr().cast(), 1), sb::kResultOk);
        fail_after(0, || {
            let (mut i, mut f, mut text, mut data, mut size) = (0, 0.0, [0u16; 2], std::ptr::null(), 0);
            assert_eq!(list.setInt(c"i".as_ptr(), 2), sb::kResultOk);
            assert_eq!(list.setFloat(c"f".as_ptr(), 0.25), sb::kResultOk);
            assert_eq!(list.getInt(c"i".as_ptr(), &mut i), sb::kResultOk);
            assert_eq!(i, 2);
            assert_eq!(list.getFloat(c"f".as_ptr(), &mut f), sb::kResultOk);
            assert_eq!(f, 0.25);
            assert_eq!(list.getString(c"s".as_ptr(), text.as_mut_ptr(), 4), sb::kResultOk);
            assert_eq!(text, [b'x' as u16, 0]);
            assert_eq!(list.getBinary(c"b".as_ptr(), &mut data, &mut size), sb::kResultOk);
            assert_eq!(size, 1);
            assert_eq!(*data.cast::<u8>(), 7);
        });
    }
}

#[test]
fn oversized_string_is_rejected_without_insertion_or_replacement() {
    let host = ComWrapper::new(HostContext::default());
    let list = create_as::<sv::IAttributeList>(&host);
    let mut text = vec![b'x' as u16; ATTR_STRING_MAX + 2];
    text[ATTR_STRING_MAX + 1] = 0;
    // SAFETY: input is terminated even beyond the cap; list and correctly sized outputs are live.
    unsafe {
        assert_eq!(list.setString(c"old".as_ptr(), [b'a' as u16, 0].as_ptr()), sb::kResultOk);
        for key in [c"old", c"new"] {
            assert_eq!(list.setString(key.as_ptr(), text.as_ptr()), sb::kResultFalse);
        }
        let mut short = [0u16; 2];
        assert_eq!(list.getString(c"old".as_ptr(), short.as_mut_ptr(), 4), sb::kResultOk);
        assert_eq!(short, [b'a' as u16, 0]);
        assert_eq!(list.getString(c"new".as_ptr(), short.as_mut_ptr(), 4), sb::kResultFalse);
        for length in [0, ATTR_STRING_MAX - 1, ATTR_STRING_MAX] {
            let mut source = vec![b'z' as u16; length];
            source.push(0);
            assert_eq!(list.setString(c"boundary".as_ptr(), source.as_ptr()), sb::kResultOk);
            let mut out = vec![u16::MAX; source.len()];
            assert_eq!(list.getString(c"boundary".as_ptr(), out.as_mut_ptr(), (out.len() * 2) as u32), sb::kResultOk);
            assert_eq!(out, source);
        }
    }
}

#[test]
fn oversized_binary_and_ids_are_rejected_before_copying() {
    let host = ComWrapper::new(HostContext::default());
    let list = create_as::<sv::IAttributeList>(&host);
    let msg = create_as::<sv::IMessage>(&host);
    let mut id = vec![b'x'; ATTR_STRING_MAX + 2];
    id[ATTR_STRING_MAX + 1] = 0;
    // SAFETY: oversized binary is rejected before access, as the method contract promises;
    // ID storage is terminated and readable. All remaining inputs are valid terminated literals.
    unsafe {
        assert_eq!(list.setBinary(c"big".as_ptr(), std::ptr::null(), (ATTR_BINARY_MAX + 1) as u32), sb::kOutOfMemory);
        assert_eq!(list.setInt(id.as_ptr().cast(), 1), sb::kResultFalse);
        msg.setMessageID(c"old".as_ptr());
        msg.setMessageID(id.as_ptr().cast());
        assert_eq!(std::ffi::CStr::from_ptr(msg.getMessageID()), c"old");
    }
}

#[test]
fn unsupported_creation_returns_false_and_clears_output() {
    let host = ComWrapper::new(HostContext::default());
    for (cid, iid) in [
        ([0xff; 16], sv::IMessage::IID),
        (sv::IMessage::IID, sv::IAttributeList::IID),
        (sv::IAttributeList::IID, sv::IMessage::IID),
        (sv::IMessage::IID, [0xff; 16]),
        (sv::IAttributeList::IID, [0xff; 16]),
    ] {
        assert_eq!(create(&host, cid, iid), (sb::kResultFalse, std::ptr::null_mut()));
        assert_eq!(fail_after(0, || create(&host, cid, iid)), (sb::kResultFalse, std::ptr::null_mut()));
    }
}

#[test]
fn query_interface_failure_clears_output() {
    let host = ComWrapper::new(HostContext::default());
    for (cid, unsupported) in [(sv::IMessage::IID, sv::IAttributeList::IID), (sv::IAttributeList::IID, sv::IMessage::IID)] {
        let (result, out) = create(&host, cid, FUnknown::IID);
        assert_eq!(result, sb::kResultOk);
        // SAFETY: successful creation transferred one FUnknown reference, adopted exactly once.
        let object = unsafe { ComPtr::<FUnknown>::from_raw(out.cast()) }.expect("object");
        for iid in [unsupported, [0xff; 16]] {
            let iid = iid.map(|b| b as c_char);
            let mut output = std::ptr::dangling_mut::<c_void>();
            // SAFETY: live object, readable IID and writable sentinel output; QI must clear failure.
            assert_eq!(unsafe { query(object.as_ptr(), &iid, &mut output) }, sb::kNoInterface);
            assert!(output.is_null());
        }
    }
}

#[test]
fn query_interface_rejects_null_arguments() {
    let host = ComWrapper::new(HostContext::default());
    for cid in [sv::IMessage::IID, sv::IAttributeList::IID] {
        let (result, out) = create(&host, cid, FUnknown::IID);
        assert_eq!(result, sb::kResultOk);
        // SAFETY: successful creation transferred one FUnknown reference, adopted exactly once.
        let object = unsafe { ComPtr::<FUnknown>::from_raw(out.cast()) }.expect("object");
        let mut output = std::ptr::dangling_mut::<c_void>();
        // SAFETY: live object; our QI wrapper explicitly accepts and rejects null arguments.
        unsafe {
            for iid in [cid, [0xff; 16]] {
                let iid = iid.map(|b| b as c_char);
                assert_eq!(query(object.as_ptr(), &iid, std::ptr::null_mut()), sb::kInvalidArgument);
            }
            assert_eq!(query(object.as_ptr(), std::ptr::null(), &mut output), sb::kInvalidArgument);
            assert!(output.is_null());
            assert_eq!(query(object.as_ptr(), std::ptr::null(), std::ptr::null_mut()), sb::kInvalidArgument);
        }
    }
}

#[test]
fn query_interface_preserves_identity_and_owns_one_reference() {
    let host = ComWrapper::new(HostContext::default());
    for cid in [sv::IMessage::IID, sv::IAttributeList::IID] {
        let (result, out) = create(&host, cid, FUnknown::IID);
        assert_eq!(result, sb::kResultOk);
        // SAFETY: adopt the single returned reference; each successful QI adds exactly one more.
        let object = unsafe { ComPtr::<FUnknown>::from_raw(out.cast()) }.expect("object");
        // SAFETY: object remains owned. Every explicit AddRef is balanced; only QI refs are released.
        unsafe {
            assert_eq!(add_ref(object.as_ptr()), 2);
            assert_eq!(release(object.as_ptr()), 1);
            for iid in [cid, FUnknown::IID] {
                let iid = iid.map(|b| b as c_char);
                let mut queried = std::ptr::null_mut();
                assert_eq!(query(object.as_ptr(), &iid, &mut queried), sb::kResultOk);
                assert_eq!(queried, out);
                assert_eq!(release(object.as_ptr()), 1);
            }
        }
    }
}

#[test]
fn retained_attributes_outlive_message() {
    let host = ComWrapper::new(HostContext::default());
    let msg = create_as::<sv::IMessage>(&host);
    // SAFETY: getAttributes borrows from the owned message. Explicit AddRef retains it before
    // adopting that extra reference. Message destruction releases only the message's reference.
    unsafe {
        let borrowed = vst3::ComRef::from_raw(msg.getAttributes()).expect("attributes");
        assert_eq!(add_ref(borrowed.as_ptr()), 2);
        let retained = ComPtr::from_raw(borrowed.as_ptr()).expect("retained attributes");
        assert_eq!(retained.setInt(c"n".as_ptr(), 42), sb::kResultOk);
        drop(msg);
        let mut n = 0;
        assert_eq!(retained.getInt(c"n".as_ptr(), &mut n), sb::kResultOk);
        assert_eq!(n, 42);
        assert_eq!(add_ref(retained.as_ptr()), 2);
        assert_eq!(release(retained.as_ptr()), 1);
    }
}

/// A destructor probe uses the same single-interface wrapper and generated refcount ABI.
struct DropSpy(Arc<AtomicUsize>);
impl Class for DropSpy {
    type Interfaces = (FUnknown,);
}
impl Drop for DropSpy {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn host_object_releases_once_across_threads_and_on_allocation_failure() {
    const VTBL: sb::FUnknownVtbl = FUnknown::make_vtbl::<DropSpy, HostObject<DropSpy>, 0>();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(fail_after(0, || try_host_object::<_, FUnknown>(DropSpy(drops.clone()), &VTBL)).is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let object = try_host_object::<_, FUnknown>(DropSpy(drops.clone()), &VTBL).expect("object");
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let address = object.as_ptr() as usize;
            scope.spawn(move || {
                // SAFETY: the parent holds a reference until every scoped worker has joined.
                // Workers only borrow the interface and balance their own extra references.
                let borrowed = unsafe { vst3::ComRef::from_raw(address as *mut FUnknown) }.expect("live object");
                for _ in 0..1000 {
                    // SAFETY: parent keeps the allocation alive; each pair owns/surrenders one ref.
                    unsafe {
                        add_ref(borrowed.as_ptr());
                        release(borrowed.as_ptr());
                    }
                }
            });
        }
    });
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let address = object.into_raw() as usize;
    std::thread::spawn(move || {
        // SAFETY: transfer the sole owned reference to this thread; final Release destroys once.
        let object = unsafe { ComPtr::from_raw(address as *mut FUnknown) }.expect("transferred object");
        // SAFETY: the live object owns one reference; the extra reference is balanced immediately.
        unsafe {
            assert_eq!(add_ref(object.as_ptr()), 2);
            assert_eq!(release(object.as_ptr()), 1);
        }
    })
    .join()
    .expect("release thread");
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

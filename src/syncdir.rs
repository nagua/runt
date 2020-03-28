use cache::maildir_flags_from_imap;
use cache::Cache;
use cache::MessageMeta;
use cache::SyncFlags;
use config::Account;
use imap::types::{Fetch, Mailbox, Uid, ZeroCopy};
use imapw::{FetchResult, Imap, UidResult};
use maildirw::Maildir;
use notify::{watcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs;
use std::ops::Deref;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::{spawn, JoinHandle};
use std::time::Duration;
use std::vec::Vec;

#[derive(Debug)]
pub enum SyncMessage {
    Exit,
    ImapChanged,
    MaildirChanged,
}

pub struct SyncDir {
    pub config: Account,
    pub mailbox: String,
    pub sender: Sender<SyncMessage>,
    receiver: Receiver<SyncMessage>,
    imap: Imap,
    cache: Cache,
    maildir: Maildir,
    idlethread: Option<JoinHandle<()>>,
    fsthread: Option<JoinHandle<()>>,
}

impl SyncDir {
    pub fn new(config: &Account, mailbox: String) -> Result<SyncDir, String> {
        let myconfig = config.clone();
        let imap = Imap::new(&myconfig)?;
        let cache = Cache::new(&myconfig.account, &mailbox).unwrap();
        let maildir = Maildir::new(&myconfig.maildir, &myconfig.account, &mailbox)?;
        let (sender, receiver) = channel();
        Ok(SyncDir {
            config: myconfig,
            mailbox,
            sender,
            receiver,
            imap,
            cache,
            maildir,
            idlethread: None,
            fsthread: None,
        })
    }

    fn idle(&self) -> Result<JoinHandle<()>, String> {
        let mut imap = Imap::new(&self.config)?;
        imap.select_mailbox(&self.mailbox.as_str())?;
        let sender = self.sender.clone();
        let handle = spawn(move || {
            if let Err(why) = imap.idle() {
                eprintln!("Error in imap IDLE: {}", why);
            }
            imap.logout().ok();
            sender.send(SyncMessage::ImapChanged).ok();
        });
        Ok(handle)
    }

    fn fswait(&self) -> Result<JoinHandle<()>, String> {
        let sender = self.sender.clone();
        let path = self.maildir.path();
        let handle = spawn(move || {
            let (tx, rx) = channel();
            let mut watcher = watcher(tx, Duration::from_secs(10)).unwrap();
            watcher.watch(path, RecursiveMode::Recursive).unwrap();
            loop {
                match rx.recv() {
                    Ok(event) => {
                        match event {
                            notify::DebouncedEvent::Write(path) if path.is_dir() => {
                                // trigger on dir writes only, which cover everything else
                                sender.send(SyncMessage::MaildirChanged).ok();
                            }
                            _ => (),
                        }
                    }
                    Err(e) => eprintln!("Maildor watch error: {:?}", e),
                }
            }
        });
        Ok(handle)
    }

    fn save_message_in_maildir(&mut self, fetch: &Fetch) -> Result<MessageMeta, String> {
        fetch
            .body()
            .ok_or_else(|| "No BODY in FETCH result".to_string())
            .and_then(|body| {
                self.maildir
                    .save_message(body, &maildir_flags_from_imap(fetch.flags()))
            })
            .and_then(|id| self.cache.add(&id, &fetch))
    }

    fn cache_message_for_uid(&mut self, uid: Uid) -> Result<(), String> {
        self.imap.fetch_uid(uid).and_then(|zc_vec_fetch| {
            for fetch in zc_vec_fetch.deref() {
                eprintln!("Fetching UID {} FLAGS {:?}", uid, fetch.flags());
                if let Err(e) = self.save_message_in_maildir(fetch) {
                    return Err(format!("Save UID {} failed: {}", uid, e));
                }
            }
            Ok(())
        })
    }

    fn update_cache_for_uid(
        &mut self,
        meta: &MessageMeta,
        uidres: &UidResult,
    ) -> Result<(), String> {
        // Check if anything has changed
        if meta.is_equal(uidres) {
            return Ok(());
        }

        if meta.needs_refetch(uidres) {
            // Pull down a whole new copy of the message.
            self.delete_message(meta.uid())?;
            self.cache_message_for_uid(meta.uid())
        } else {
            println!("Updating UID {}", uidres.uid());
            self.cache.update(uidres).and_then(|newmeta| {
                if meta.needs_move_from_new_to_cur(uidres)
                    && self.maildir.message_is_in_new(meta.id())?
                {
                    println!("Moving {} {} from new to cur", meta.uid(), meta.id());
                    self.maildir
                        .move_message_to_cur(meta.id(), &newmeta.flags())
                } else {
                    self.maildir
                        .set_flags_for_message(newmeta.id(), &newmeta.flags())
                }
            })
        }
    }

    fn cache_uids(&mut self, zc_vec_fetch: &ZeroCopy<Vec<Fetch>>) -> Result<(), String> {
        let mut err = false;
        for fetch in zc_vec_fetch.deref() {
            match FetchResult::from(fetch) {
                FetchResult::Uid(uidres) => {
                    let uid = uidres.uid();
                    let res = if let Ok(meta) = self.cache.get_uid(uid) {
                        self.update_cache_for_uid(&meta, &uidres)
                    } else {
                        self.cache_message_for_uid(uid)
                    };
                    if let Err(e) = res {
                        eprintln!("Cache UID {} failed: {}", uid, e);
                        err = true;
                    }
                }
                FetchResult::Other(f) => println!("Got Other: {:?}", f),
            }
        }
        if err {
            Err("Cache failed".to_string())
        } else {
            Ok(())
        }
    }

    fn delete_message(&self, uid: u32) -> Result<(), String> {
        self.cache
            .get_uid(uid)
            .and_then(|meta| self.maildir.delete_message(meta.id()))
            .and_then(|_| self.cache.delete_uid(uid))
    }

    fn remove_absent_uids(&mut self, zc_vec_fetch: &ZeroCopy<Vec<Fetch>>) -> Result<(), String> {
        let mut err = false;
        self.cache.get_known_uids().and_then(|mut cached_uids| {
            // Remove all the fetched uids from the cached values
            // leaving only uids that are in the cache but not on
            // the server anymore.
            for fetch in zc_vec_fetch.deref() {
                match FetchResult::from(fetch) {
                    FetchResult::Uid(uidres) => {
                        let uid = uidres.uid();
                        if !cached_uids.remove(&uid) {
                            eprintln!("UID {} exists on server but not in cache", uid);
                            err = true;
                        }
                    }
                    FetchResult::Other(f) => println!("Got Other: {:?}", f),
                }
            }

            // Remove uids from cache that have been removed on the server
            for uid in cached_uids {
                println!("UID {} is gone on server", uid);
                if let Err(e) = self.delete_message(uid) {
                    eprintln!("Error deleting UID {}: {}", uid, e);
                    err = true;
                }
            }

            if err {
                Err("Error removing absent UIDs".to_string())
            } else {
                Ok(())
            }
        })
    }

    fn refresh_cache(&mut self, last_seen_uid: u32, mailbox: &Mailbox) -> Result<(), String> {
        let end: Option<u32> = match last_seen_uid {
            0 => None,
            x => Some(x),
        };

        println!("Fetching UIDs 1:{:?}", end);
        self.imap.fetch_uids(1, end).and_then(|zc_vec_fetch| {
            if !self.cache.is_valid(mailbox) {
                // We have a new state, so delete the existing one
                self.cache.delete_maildir_state()
            } else {
                Ok(())
            }
            .and_then(|_| self.cache_uids(&zc_vec_fetch))
            .and_then(|_| self.remove_absent_uids(&zc_vec_fetch))
            .and_then(|_| self.cache.update_imap_state(mailbox))
        })
    }

    fn get_new_messages(&mut self, uid: u32) -> Result<(), String> {
        self.imap
            .fetch_uids(uid, None)
            .and_then(|zc_vec_fetch| self.cache_uids(&zc_vec_fetch))
    }

    fn push_maildir_changes(&mut self) -> Result<(), String> {
        let mut ids = self.cache.get_known_ids()?;
        let (new, changed) = self.maildir.get_updates(&mut ids)?;
        let mut refetch = HashSet::<u32>::new();

        // ids now contains maildir entries that are in the cache
        // but not on the file system anymore. They need to be deleted
        // from the server.
        for meta in ids.values() {
            // delete from server
            self.imap.delete_uid(meta.uid())?;
            // delete from cache
            self.cache.delete_uid(meta.uid())?;
            // the change will come back to us on the IDLE
            // thread, but we'll just ignore it.
        }

        // changed contains maildir entries that are different on
        // disk than in the cache. These need to be synchronized
        // to the server.
        for id in changed {
            let cache_v = self.cache.get_id(&id)?;
            let mail_v = self.maildir.get_id(&id)?;

            // If we need to update flags then send changes.
            let cache_flags = SyncFlags::from(cache_v.flags().as_str());
            let maildir_flags = SyncFlags::from(mail_v.flags());
            let flags_diff = cache_flags.diff(maildir_flags);
            if let Some(flags) = flags_diff.add.as_imap_flags() {
                self.imap.add_flags_for_uid(cache_v.uid(), &flags)?;
                refetch.insert(cache_v.uid());
            }
            if let Some(flags) = flags_diff.sub.as_imap_flags() {
                self.imap.remove_flags_for_uid(cache_v.uid(), &flags)?;
                refetch.insert(cache_v.uid());
            }

            // If we need to push a new body.
            // FIXME: Can we use something better than size?
            //        If we store the file mod date, we could
            //        use that instead...
            if cache_v.size() as u64 != mail_v.size() {
                self.imap.replace_uid(
                    cache_v.uid(),
                    &fs::read(mail_v.path()).map_err(|e| e.to_string())?,
                )?;
                self.maildir.delete_message(&id)?;
                self.cache.delete_uid(cache_v.uid())?;
                refetch.remove(&cache_v.uid());
            }
        }

        // new contains maildir entries that are on the file system
        // but not in the cache. These need to be sent to the server.
        for id in new {
            let mail_v = self.maildir.get_id(&id)?;
            // Push to the server first, then delete the local copy
            self.imap
                .append(&fs::read(mail_v.path()).map_err(|e| e.to_string())?, None)?;
            // These will come back to us on the idle loop,
            // at which time they will get cache entries.
            self.maildir.delete_message(&id)?;
        }

        for uid in refetch {
            self.imap
                .fetch_uid_meta(uid)
                .and_then(|zc_vec_fetch| self.cache_uids(&zc_vec_fetch))?;
        }

        self.cache.update_maildir_state()
    }

    pub fn sync(&mut self) {
        //self.imap.debug(true);
        //self.imap.enable_qresync().unwrap();
        //self.imap.debug(true);
        self.imap
            .select_mailbox(&self.mailbox.as_str())
            .and_then(|mailbox| {
                // FIXME: HIGHESTMODSEQ support, will trigger VANISHED responses
                loop {
                    let last_seen_uid = self.cache.get_last_seen_uid();
                    let res = self
                        .refresh_cache(last_seen_uid, &mailbox)
                        .and_then(|_| self.push_maildir_changes())
                        .and_then(|_| self.get_new_messages(last_seen_uid + 1));

                    if let Err(e) = res {
                        eprintln!("Error syncing: {}", e);
                        break;
                    };

                    if self.idlethread.is_none() {
                        match self.idle() {
                            Ok(handle) => self.idlethread = Some(handle),
                            Err(why) => {
                                eprintln!("Error in IDLE: {}", why);
                                break;
                            }
                        }
                    }

                    if self.fsthread.is_none() {
                        match self.fswait() {
                            Ok(handle) => self.fsthread = Some(handle),
                            Err(why) => {
                                eprintln!("Error in watching file system: {}", why);
                                break;
                            }
                        }
                    }

                    match self.receiver.recv() {
                        Ok(SyncMessage::Exit) => break,
                        Ok(SyncMessage::ImapChanged) => {
                            println!("IMAP changed");
                            if self.idlethread.is_some() {
                                self.idlethread.take().unwrap().join().ok();
                            }
                        }
                        Ok(SyncMessage::MaildirChanged) => {
                            println!("Maildir changed");
                        }
                        Err(why) => {
                            eprintln!("Error in recv(): {}", why);
                            break;
                        }
                    }
                }
                Ok(())
            })
            .unwrap();

        self.imap.logout().unwrap();
    }
}

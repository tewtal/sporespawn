extern crate discord;
extern crate byteorder;

use byteorder::{LittleEndian, ReadBytesExt};
use discord::{Discord, State};
use discord::model::{ChannelId, Event};
use std::ascii::AsciiExt;
use discord::voice::{AudioSource};
use std::sync::{Mutex, Arc};
use std::sync::atomic::{Ordering, AtomicBool};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Result as IoResult};
use std::process::{Child, Command, Stdio};

pub struct ChildContainer(Child);

impl Read for ChildContainer 
{
    fn read(&mut self, buffer: &mut [u8]) -> IoResult<usize> {
        self.0.stdout.as_mut().unwrap().read(buffer)
    }
}

impl Drop for ChildContainer
{
    fn drop(&mut self)
    {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub struct Song
{
    link: String,
    title: String,
    user: String,
    duration: String
}

enum PlayingState
{
    New,
    Idle,
    Playing
}

pub struct QueuedSource
{
    discord: Arc<Discord>,
    queue: Arc<Mutex<VecDeque<Song>>>,
    channel: ChannelId,
    stereo: bool,
    buffer: Option<ChildContainer>,
    skip: Arc<AtomicBool>,
    state: PlayingState
}

impl AudioSource for QueuedSource
{
    fn is_stereo(&mut self) -> bool { self.stereo }
    fn read_frame(&mut self, buffer: &mut [i16]) -> Option<usize>
    {
        match self.state
        {
            PlayingState::New => 
            {
                self.state = PlayingState::Idle; 
            },
            PlayingState::Idle =>
            {
                if self.buffer.is_some()
                {
                    self.buffer = None;
                }

                let queue = self.queue.lock().unwrap();
                if queue.len() > 0
                {
                    if let Some(song) = queue.get(0)
                    {
                        let _ = self.discord.send_message(self.channel, &format!("Now Playing: *{}* [{}] requested by **{}**", song.title, song.duration, song.user), "", false);
        
                        if let Ok(stream) = open_mpv_stream(&song.link)
                        {
                            self.buffer = Some(stream);
                        }
                        else
                        {
                            return Some(0);
                        }

                        self.state = PlayingState::Playing;
                    }
                }
                else
                {
                    return Some(0);
                }
            },
            PlayingState::Playing =>
            {
                
                if self.skip.load(Ordering::Relaxed) == true
                {
                    self.skip.store(false, Ordering::Relaxed);
                    self.state = PlayingState::Idle;
                    let _ = self.queue.lock().unwrap().pop_front();
                    return Some(0);
                }
   
                if let Some(ref mut buf) = self.buffer
                {
                    for (_, val) in buffer.iter_mut().enumerate()                    
                    {
                        *val = match buf.read_i16::<LittleEndian>()                    
                        {
                            Ok(val) => val,
                            Err(_) =>
                            {
                                // Read error, stop playing
                                self.state = PlayingState::Idle;
                                let _ = self.queue.lock().unwrap().pop_front();
                                return Some(0);
                            }
                        }
                    }
                }

                return Some(buffer.len());
             }
        };

        Some(0)
    }
}

pub fn open_mpv_stream(url: &str) -> Result<ChildContainer, std::io::Error>
{
    let mut uri = url.to_owned();

    if !uri.starts_with("http")
    {
        uri.insert_str(0, "ytdl://");
    }

    let args = [
        "--really-quiet",
        "--ytdl-raw-options=default-search=ytsearch:format=bestaudio/best",
        "--no-video",
        "--o=-",
        "--of=s16le",
        "--audio-samplerate=48000",
        "--af=lavrresample",
        "--audio-channels=stereo",
        "--audio-stream-silence=yes",
        "--audio-wait-open=1",
        "--volume=75",
        "--cache=yes",
        "--cache-secs=2",        
    ];

    let out = Command::new("mpv")
        .arg(uri)
        .args(&args)
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;

    Ok(ChildContainer(out))
}

pub fn create_queued_source(discord: Arc<Discord>, queue: Arc<Mutex<VecDeque<Song>>>, channel: ChannelId, skip: Arc<AtomicBool>) -> Box<AudioSource>
{
    let q = QueuedSource
    {
        discord: discord,
        queue: queue,
        channel: channel,
        stereo: true,
        buffer: None,
        skip: skip,
        state: PlayingState::New
    };

    Box::new(q)
}

pub fn get_song_info(url: String, user: String) -> Result<Song, std::io::Error>
{
    let args = [
        "--no-warnings",
        "-e",
        "--get-duration",
        "--no-playlist",
        "--skip-download",
        "--default-search=ytsearch",
        &url,
    ];
    
    let out = Command::new("youtube-dl")
        .args(&args)
        .stdin(Stdio::null())
        .output()?;
    
    if !out.status.success() 
    {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, ""));
    }
    

    let s = String::from_utf8(out.stdout).unwrap();
    let mut info = s.lines();
    let title = info.next().unwrap_or("");
    let duration = info.next().unwrap_or("");

    let s = Song 
    {
        link: url.clone(),
        title: title.to_owned(),
        duration: duration.to_owned(),
        user: user
    };

    Ok(s)
}

pub fn main()
{
    let discord = Arc::new(Discord::from_bot_token("<DISCORD BOT TOKEN>").expect("login failed"));
    let (mut connection, ready) = discord.connect().expect("connect failed");

    println!("Sporespawn is ready to rock! Logged in as {}", ready.user.username);

    let mut state = State::new(ready);
    let mut queues = HashMap::new();
    let mut skips = HashMap::new();

    connection.set_game_name(String::from("Music"));

    loop
    {
        let event = match connection.recv_event()
        {
            Ok(event) => event,
            Err(err) =>
            {
                println!("OUCH! Receive error: {:?}", err);
                if let discord::Error::WebSocket(..) = err
                {
                    let (new_connection, ready) = discord.connect().expect("connect failed");
                    connection = new_connection;
                    
                    state = State::new(ready);
                    println!("We're back in business!");
                }

                if let discord::Error::Closed(..) = err
                {
                    break
                }
                continue
            },
        };

        state.update(&event);

        match event
        {
            Event::MessageCreate(message) =>
            {                
                if message.author.id == state.user().id
                {
                    continue
                }

                let mut split = message.content.splitn(2, ' ');
                let first_word = split.next().unwrap_or("");
                let argument = split.next().unwrap_or("");

                let vchan = state.find_voice_user(message.author.id);                

                if first_word.eq_ignore_ascii_case("%stop")
                {
                    vchan.map(|(sid, _)| connection.voice(sid).stop());
                }
                else if first_word.eq_ignore_ascii_case("%quit")
                {
                    vchan.map(|(sid, _)| connection.drop_voice(sid));                    
                }
                else if first_word.eq_ignore_ascii_case("%join")
                {
                    if let Some((server_id, channel_id)) = vchan
                    {            
                        let voice = connection.voice(server_id);
                        voice.set_deaf(true);
                        voice.connect(channel_id);
    
                        let queue: VecDeque<Song> = VecDeque::new();
                        let shared_queue = Arc::new(Mutex::new(queue));

                        if queues.contains_key(&message.channel_id)
                        {
                            queues.remove(&message.channel_id);
                        }                        
                        queues.insert(message.channel_id.clone(), shared_queue.clone());

                        let skip = Arc::new(AtomicBool::new(false));
                        if skips.contains_key(&message.channel_id)
                        {
                            skips.remove(&message.channel_id);
                        }
                        skips.insert(message.channel_id.clone(), skip.clone());

                        let src = create_queued_source(discord.clone(), shared_queue.clone(), message.channel_id, skip.clone());
                        voice.play(src);
                    }
                    else
                    {
                        let _ = discord.send_message(message.channel_id, "You must be in a voice channel", "", false);
                    }
                }
                else if first_word.eq_ignore_ascii_case("%play")
                {
                    if let Some(q) = queues.get(&message.channel_id)
                    {
                        if let Ok(song) = get_song_info(argument.to_owned(), message.author.name)
                        {
                            let _ = discord.send_message(message.channel_id, &format!("Enqueued: *{}* [{}]", song.title, song.duration), "", false);
                            q.lock().unwrap().push_back(song);
                        }
                        else
                        {
                            let _ = discord.send_message(message.channel_id, "Sorry, could not find that song", "", false);
                        }
                    }
                }
                else if first_word.eq_ignore_ascii_case("%skip")
                {
                    if let Some(skip) = skips.get(&message.channel_id)
                    {
                        skip.store(true, Ordering::Relaxed);
                        let _ = discord.send_message(message.channel_id, "Skipped to the next song", "", false);
                    }
                }
                else if first_word.eq_ignore_ascii_case("%undo")
                {
                    if let Some(q) = queues.get(&message.channel_id)
                    {
                        if q.lock().unwrap().len() > 1
                        {
                            let s = q.lock().unwrap().pop_back().unwrap();
                            let _ = discord.send_message(message.channel_id, &format!("Removed *{}* from the queue", s.title), "", false);
                        }
                        else
                        {
                            let _ = discord.send_message(message.channel_id, "The queue is already empty", "", false);
                        }
                    }
                }
                else if first_word.eq_ignore_ascii_case("%list")
                {
                    if let Some(q) = queues.get(&message.channel_id)
                    {
                        let mut s = "**Song queue:**\n".to_string();
                        for song in q.lock().unwrap().iter().skip(1)
                        {
                            s.push_str(&format!("*{}* [{}] requested by **{}**\n", song.title, song.duration, song.user));
                        }
                        let _ = discord.send_message(message.channel_id, &s, "", false);
                    }
                }
                else if first_word.eq_ignore_ascii_case("%now")
                {
                    if let Some(q) = queues.get(&message.channel_id)
                    {
                        if let Some(song) = q.lock().unwrap().get(0)
                        {
                            let _ = discord.send_message(message.channel_id, &format!("Now Playing: *{}* [{}] requested by **{}**", song.title, song.duration, song.user), "", false);
                        }
                        else
                        {
                            let _ = discord.send_message(message.channel_id, "No song is currently playing.", "", false);
                        }
                    }
                }
                else if first_word.eq_ignore_ascii_case("%help")
                {
                    let _ = discord.send_message(message.channel_id, r#"Quick Sporespawn Guide:
**%join** - *Joins Sporespawn to the voice channel you are currently in*
**%play <url/youtube-id/search string>** - *Plays the selected song*
**%undo** - *Removes the latest queued song from the queue*
**%skip** - *Skips the currently playing song*
**%list** - *Lists the current song queue*
**%now** - *Shows the currently playing song*
**%quit** - *Stops any music playing and makes Sporespawn leave the voice channel*"#, "", false);
                }
            }            
            _ => 
            {
            },
        }
    }
}

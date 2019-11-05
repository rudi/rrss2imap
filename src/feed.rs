use chrono::{DateTime, NaiveDateTime, NaiveDate, Utc};

use super::config::*;

use super::message::*;
use super::settings::*;
use super::syndication;
use atom_syndication::Entry as AtomEntry;
use atom_syndication::Feed as AtomFeed;
use rss::Channel as RssChannel;
use rss::Item as RssItem;
use url::Url;
use unidecode::unidecode;
use regex::Regex;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Feed {
    pub url: String,
    #[serde(skip_serializing_if = "Config::is_none", default = "Config::new")]
    pub config: Config,
    #[serde(default = "Feed::at_epoch")]
    pub last_updated: NaiveDateTime,
}

impl Feed {
    /// Creates a new naivedatetime with a default value (which is, to my mind) a sensible default for computers
    pub fn at_epoch() -> NaiveDateTime {
        NaiveDateTime::from_timestamp(0, 0)
    }

    pub fn at_end_of_universe() -> NaiveDateTime {
        NaiveDate::from_ymd(9999, 1, 1).and_hms_milli(0, 0, 0, 0)
    }

    // Convert the parameters vec into a valid feed (if possible)
    pub fn from(parameters: Vec<String>) -> Feed {
        let mut consumed = parameters.clone();
        let url: String = consumed
            .pop()
            .expect("You must at least define an url to add.");
        let mut email: Option<String> = None;
        let mut folder: Option<String> = None;
        // If there is a second parameter, it can be either email or folder
        if !consumed.is_empty() {
            let second = consumed.pop().unwrap();
            // If second parameters contains an @, I suppose it is an email address
            if second.contains('@') {
                debug!(
                    "Second add parameter {} is considered an email address",
                    second
                );
                email = Some(second)
            } else {
                warn!("Second add parameter {} is NOT considered an email address, but a folder. NO MORE ARGUMENTS WILL BE PROCESSED", second);
                folder = Some(second)
            }
        }
        // If there is a third parameter, it is the folder.
        // But if folder was already defined, there is an error !
        if !consumed.is_empty() && folder == None {
            folder = Some(consumed.pop().unwrap());
        }
        Feed {
            url,
            config: Config {
                email,
                folder,
                from: None,
                inline_image_as_data: false,
            },
            last_updated: Feed::at_epoch(),
        }
    }

    pub fn to_string(&self, config: &Config) -> String {
        return format!("{} {}", self.url, self.config.clone().to_string(config));
    }

    pub fn read(&self, settings: &Settings) -> Feed {
        info!("Reading feed from {}", self.url);
        match reqwest::get(&self.url) {
            Ok(mut response) => match response.text() {
                Ok(text) => match text.parse::<syndication::Feed>() {
                    Ok(parsed) => {
                        return match parsed {
                            syndication::Feed::Atom(atom_feed) => {
                                self.read_atom(atom_feed, settings)
                            }
                            syndication::Feed::RSS(rss_feed) => {
                                self.read_rss(rss_feed, settings)
                            }
                        }
                    }
                    Err(e) => error!("Content ar {} is neither Atom, nor RSS {}.\nTODO check real content type to help user.", &self.url, e),
                },
                Err(e) => error!("There is no text at {} due to error {}", &self.url, e),
            },
            Err(e) => error!("Unable to get {} due to {}.\nTODO Add better http response analysis !", &self.url, e),
        }
        self.clone()
    }

    fn read_atom(&self, feed: AtomFeed, settings: &Settings) -> Feed {
        debug!("reading ATOM feed {}", &self.url);
        let feed_date_text = feed.updated();
        let feed_date = if feed_date_text.is_empty() {
            Feed::at_end_of_universe()
        } else {
            feed_date_text.parse::<DateTime<Utc>>().unwrap().naive_utc()
        };
        info!(
            "Feed date is {} while previous read date is {}",
            feed_date, self.last_updated
        );
        if feed_date > self.last_updated {
            info!("There should be new entries, parsing HTML content");
            feed.entries()
                .iter()
                .map(|e| extract_from_atom(e, &feed))
                .filter(|e| e.last_date >= self.last_updated)
                .for_each(|e| if !settings.do_not_save { e.write_to_imap(&self, settings) } );
            return Feed {
                url: self.url.clone(),
                config: self.config.clone(),
                last_updated: if settings.do_not_save {
                    warn!("do_not_save is set. As a consequence, feed won't be updated");
                    self.last_updated
                } else {
                    feed_date
                },
            };
        }
        self.clone()
    }

    fn read_rss(&self, feed: RssChannel, settings: &Settings) -> Feed {
        debug!("reading RSS feed {}", &self.url);
        let n = Utc::now();
        let feed_date_text = match feed.pub_date() {
            Some(p) => p.to_owned(),
            None => match feed.last_build_date() {
                Some(l) => l.to_owned(),
                None => n.to_rfc2822(),
            },
        };
        let feed_date = DateTime::parse_from_rfc2822(&feed_date_text)
            .unwrap()
            .naive_utc();
        info!(
            "Feed date is {} while previous read date is {}",
            feed_date, self.last_updated
        );
        if feed_date > self.last_updated {
            info!("There should be new entries, parsing HTML content");
            feed.items()
                .iter()
                .map(|e| extract_from_rss(e, &feed))
                .filter(|e| e.last_date >= self.last_updated)
                .for_each(|e| if !settings.do_not_save { e.write_to_imap(&self, settings) } );
            return Feed {
                url: self.url.clone(),
                config: self.config.clone(),
                last_updated: if settings.do_not_save {
                    warn!("do_not_save is set. As a consequence, feed won't be updated");
                    self.last_updated
                } else {
                    feed_date
                },
            };
        }
        self.clone()
    }
}

fn extract_authors_from_rss(entry: &RssItem, feed: &RssChannel) -> Vec<String> {
    let domain = find_rss_domain(feed);
    // This is where we also transform author names into urls in order
    // to have valid email addresses everywhere
    let message_authors: Vec<String>;
    match entry.author() {
        Some(l) => message_authors = vec![l.to_owned()],
        _ => message_authors = vec![feed.title().to_owned()],
    }
    sanitize_message_authors(message_authors, domain)
}
fn find_rss_domain(feed: &RssChannel) -> String {
    return Some(feed.link())
        .map(|href| Url::parse(href).unwrap())
        // then get host
        .map(|url| url.host_str().unwrap().to_string())
        // and return value
        .unwrap_or("todo.find.domain.atom".to_string());
}

fn extract_from_rss(entry: &RssItem, feed: &RssChannel) -> Message {
    let authors = extract_authors_from_rss(entry, feed);
    let content = entry
        .content()
        .unwrap_or_else(|| entry.description().unwrap_or(""))
        // First step is to fix HTML, so load it using html5ever
        // (because there is no better html parser than a real browser one)
        // TODO implement image inlining
        .to_owned();
    let links = match entry.link() {
        Some(l) => vec![l.to_owned()],
        _ => vec![],
    };
    let id = if links.is_empty() {
        match entry.guid() {
            Some(g) => g.value().to_owned(),
            _ => "no id".to_owned(),
        }
    } else {
        links[0].clone()
    };
    let last_date = extract_date_from_rss(entry);
    let message = Message {
        authors: authors,
        content: content,
        id: id,
        last_date: last_date,
        links: links,
        title: entry.title().unwrap_or("").to_owned(),
    };
    return message;
}

fn extract_date_from_rss(entry: &RssItem) -> NaiveDateTime {
    if entry.pub_date().is_some() {
        let mut pub_date = entry.pub_date().unwrap().to_owned();
        pub_date = pub_date.replace("UTC", "UT");
        return rfc822_sanitizer::parse_from_rfc2822_with_fallback(&pub_date).unwrap_or_else(|e| {
            panic!(
                "pub_date for item {:?} (value is {:?}) can't be parsed due to error {:?}",
                &entry, pub_date, e
                )
            }) .naive_utc();
    } else if entry.dublin_core_ext().is_some()
        && entry.dublin_core_ext().unwrap().dates().len() > 0
    {
        let pub_date = &entry.dublin_core_ext().unwrap().dates()[0];
        return DateTime::parse_from_rfc3339(&pub_date)
            .unwrap_or_else(|e| {
                panic!(
                    "dc:pub_date for item {:?} (value is {:?}) can't be parsed.due to error {:?}",
                    &entry, pub_date, e
                )
            })
            .naive_utc();
    } else {
        panic!(
            "feed item {:?} can't be parsed, as it doesn't have neither pub_date nor dc:pub_date",
            &entry
        );
    }
}

fn extract_authors_from_atom(entry: &AtomEntry, feed: &AtomFeed) -> Vec<String> {
    let domain = find_atom_domain(feed);
    // This is where we also transform author names into urls in order
    // to have valid email addresses everywhere
    let mut message_authors: Vec<String> = entry
        .authors()
        .iter()
        .map(|a| a.name().to_owned())
        .collect();
    if message_authors.is_empty() {
        message_authors = vec![feed.title().to_owned()]
    }
    sanitize_message_authors(message_authors, domain)
}

fn sanitize_message_authors(message_authors:Vec<String>, domain:String)->Vec<String> {
    let fixed = message_authors
        .iter()
        .map(|author| {
            sanitize_email(author, &domain)
        })
        .collect();
    return fixed;
}

fn find_atom_domain(feed: &AtomFeed) -> String {
    return feed
        .links()
        .iter()
        .filter(|link| link.rel() == "self" || link.rel() == "alternate")
        .next()
        // Get the link
        .map(|link| link.href())
        // Transform it into an url
        .map(|href| Url::parse(href).unwrap())
        // then get host
        .map(|url| url.host_str().unwrap().to_string())
        // and return value
        .unwrap_or("todo.find.domain.rss".to_string());
}

fn extract_from_atom(entry: &AtomEntry, feed: &AtomFeed) -> Message {
    let authors = extract_authors_from_atom(entry, feed);
    let last_date = entry
        .updated()
        .parse::<DateTime<Utc>>()
        .unwrap()
        .naive_utc();
    let content = match entry.content() {
        Some(content) => content.value().unwrap(),
        None => match entry.summary() {
            Some(summary) => summary,
            None => "",
        },
    }
    .to_owned();
    let message = Message {
        authors: authors,
        content: content,
        id: entry.id().to_owned(),
        last_date: last_date,
        links: entry.links().iter().map(|l| l.href().to_owned()).collect(),
        title: entry.title().to_owned(),
    };
    return message;
}

fn trim_to_chars(text:&str, characters:Vec<&str>)->String {
    let mut remaining = text;
    for cutter in characters {
        let elements:Vec<&str> = remaining.split(cutter).collect();
        remaining = elements[0].trim();
    }
    remaining.to_string()
}

fn sanitize_email(email:&String, domain:&String)->String {
    lazy_static! {
        static ref email_and_name_detector:Regex = 
            Regex::new("([[:alpha:]_%\\+\\-\\.]+@[[:alpha:]_%\\+\\-]+\\.[[:alpha:]_%\\+\\-]+{1,}) \\(([^\\)]*)\\)").unwrap();
    }
    if email_and_name_detector.is_match(email) {
        let captures = email_and_name_detector.captures(email).unwrap();
        return format!("{} <{}>", captures.get(2).unwrap().as_str(), captures.get(1).unwrap().as_str());
    } else {
        let email = trim_to_chars(email, vec!["|", ":", "-"]);
        let tuple = (email.clone(),
        unidecode(&email).to_lowercase() // second element of tuple is generated user address
                    .replace(" ", "_")
                );
        return format!("{} <{}@{}>", tuple.0, tuple.1, domain);
    }
}

#[cfg(test)]
mod tests {
    mod email_tests {
        use super::super::*;

        #[test]
        fn can_create_email_from_xkcd() {
            assert_eq!("xkcd.com <xkcd.com@xkcd.com>", sanitize_email(&"xkcd.com".to_string(), &"xkcd.com".to_string()));
        }

        #[test]
        fn can_create_email_from_sex_at_liberation() {
            assert_eq!("sexes.blogs.liberation.fr <sexes.blogs.liberation.fr@sexes.blogs.liberation.fr>", 
                sanitize_email(
                    &"sexes.blogs.liberation.fr - Derniers articles".to_string(), 
                    &"sexes.blogs.liberation.fr".to_string()));
        }

        #[test]
        fn can_create_email_from_real_address_at_sex_at_liberation() {
            assert_eq!("Agnès Giard <aniesu.giard@gmail.com>", 
                sanitize_email(
                    &"aniesu.giard@gmail.com (Agnès Giard)".to_string(), 
                    &"sexes.blogs.liberation.fr".to_string()));
        }
    }
}
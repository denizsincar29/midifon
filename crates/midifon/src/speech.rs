//! Озвучка сообщений через скринридер (NVDA, JAWS и др.) библиотекой prism (обёртка prismer).
//! Если подходящего бэкенда нет, всё молча работает без речи (сообщения остаются в статусной строке).
use prismer::{Backend, Prism};

pub struct Speech {
    backend: Option<Backend<'static>>,
    name: String,
}

impl Speech {
    pub fn new() -> Speech {
        // Prism живёт до конца процесса, поэтому его можно «утечь» ради 'static
        let prism: &'static Prism = match Prism::new() {
            Ok(p) => Box::leak(Box::new(p)),
            Err(_) => return Speech { backend: None, name: String::new() },
        };
        match prism.create_best() {
            Ok(b) => {
                let name = b.name();
                Speech { backend: Some(b), name }
            }
            Err(_) => Speech { backend: None, name: String::new() },
        }
    }

    /// Название активного бэкенда (например, NVDA) или None.
    pub fn backend_name(&self) -> Option<&str> {
        self.backend.as_ref().map(|_| self.name.as_str())
    }

    /// Говорит, прерывая предыдущую фразу: сообщения всегда следуют за действием пользователя.
    pub fn speak(&self, text: &str) {
        if let Some(b) = &self.backend {
            let _ = b.speak(text, true);
        }
    }
}
